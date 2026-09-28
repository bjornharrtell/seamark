//! Database-side execution of validated read plans with SeaORM.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, IdenStatic, ModelTrait, Order,
    QueryFilter, QueryOrder, QuerySelect, Value,
    sea_query::{ColumnType, IntoCondition},
};
use serde_json::Value as JsonValue;

use crate::document::RelationshipData;
use crate::http::{
    AdapterIncludedResource, AdapterResource, QueryAdapterError, QueryCollectionResult,
    QueryResourceAdapter, QueryResourceResult,
};
use crate::limits::ExecutionLimits;
use crate::projection::{include_relationships_by_type, project_adapter_record_with_includes};
use crate::query::{
    FilterExpression, FilterValue, IncludeNode, Page, PlannedField, ReadPlan, SortDirection,
};
use crate::registry::{
    AttributeMapping, AttributePermission, RelationshipMapping, RelationshipReassignment,
    RelationshipStorage, ResourceDefinition, ResourceRegistry,
};

type SeaOrmResourceMapper<Model> =
    Arc<dyn Fn(&Model) -> Result<AdapterResource, String> + Send + Sync>;
type SeaOrmComputedValueMapper<Model> =
    Arc<dyn Fn(&Model) -> Result<JsonValue, String> + Send + Sync>;

/// Encodes parsed query string literals as typed SeaORM values.
///
/// Implementations should validate values for the mapped entity field and
/// return an error for unsupported or malformed literals.
pub trait SeaOrmFilterValueCodec: Send + Sync {
    /// Converts a non-null query literal for a mapped model field.
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String>;

    /// Converts a public resource identifier for its mapped model field.
    ///
    /// Implementations with typed identifiers should override this method.
    /// The default preserves compatibility for string-backed identifiers.
    fn encode_resource_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        self.encode_filter_value(model_field, value)
    }
}

impl<C> SeaOrmFilterValueCodec for Arc<C>
where
    C: SeaOrmFilterValueCodec + ?Sized,
{
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        self.as_ref().encode_filter_value(model_field, value)
    }

    fn encode_resource_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        self.as_ref().encode_resource_identifier(model_field, value)
    }
}

/// Encodes mutation values and converts database identifiers to API strings.
pub trait SeaOrmMutationValueCodec: Send + Sync {
    /// Converts a validated JSON value for a mapped model field.
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String>;

    /// Encodes a JSON:API identifier string using the mapped database type.
    fn encode_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        self.encode_mutation_value(model_field, &JsonValue::String(value.to_owned()))
    }

    /// Converts a database identifier value to its public string form.
    fn decode_identifier(&self, model_field: &str, value: &Value) -> Result<String, String>;
}

impl<C> SeaOrmMutationValueCodec for Arc<C>
where
    C: SeaOrmMutationValueCodec + ?Sized,
{
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        self.as_ref().encode_mutation_value(model_field, value)
    }

    fn decode_identifier(&self, model_field: &str, value: &Value) -> Result<String, String> {
        self.as_ref().decode_identifier(model_field, value)
    }

    fn encode_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        self.as_ref().encode_identifier(model_field, value)
    }
}

/// A codec suitable for both SeaORM query filtering and mutation handling.
pub trait SeaOrmValueCodec: SeaOrmFilterValueCodec + SeaOrmMutationValueCodec {}

impl<T> SeaOrmValueCodec for T where T: SeaOrmFilterValueCodec + SeaOrmMutationValueCodec {}

/// A schema-aware codec for common SeaORM scalar columns and identifiers.
///
/// It handles booleans, signed and unsigned integers, floats, strings, chars,
/// JSON, and UUIDs, including SQL `NULL` for nullable fields. Other column
/// types fail explicitly and can be supported with an application codec.
pub struct SeaOrmColumnValueCodec<E>(PhantomData<fn() -> E>);

impl<E> Default for SeaOrmColumnValueCodec<E> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<E> SeaOrmColumnValueCodec<E>
where
    E: EntityTrait,
    E::Column: FromStr + ColumnTrait,
{
    fn column_type(model_field: &str) -> Result<ColumnType, String> {
        let column = E::Column::from_str(model_field)
            .map_err(|_| format!("model field `{model_field}` is not a SeaORM column"))?;
        Ok(column.def().get_column_type().clone())
    }

    fn encode_literal(model_field: &str, literal: &str) -> Result<Value, String> {
        let column_type = Self::column_type(model_field)?;
        match column_type {
            ColumnType::Char(_) => {
                let mut chars = literal.chars();
                match (chars.next(), chars.next()) {
                    (Some(character), None) => Ok(Value::Char(Some(character))),
                    _ => Err("expected a one-character string literal".to_owned()),
                }
            }
            ColumnType::String(_) | ColumnType::Text => Ok(Value::String(Some(literal.to_owned()))),
            ColumnType::TinyInteger => parse(literal).map(Value::TinyInt),
            ColumnType::SmallInteger => parse(literal).map(Value::SmallInt),
            ColumnType::Integer => parse(literal).map(Value::Int),
            ColumnType::BigInteger => parse(literal).map(Value::BigInt),
            ColumnType::TinyUnsigned => parse(literal).map(Value::TinyUnsigned),
            ColumnType::SmallUnsigned => parse(literal).map(Value::SmallUnsigned),
            ColumnType::Unsigned => parse(literal).map(Value::Unsigned),
            ColumnType::BigUnsigned => parse(literal).map(Value::BigUnsigned),
            ColumnType::Float => parse(literal).map(Value::Float),
            ColumnType::Double => parse(literal).map(Value::Double),
            ColumnType::Decimal(_) => literal
                .parse::<rust_decimal::Decimal>()
                .map(|value| Value::Decimal(Some(value)))
                .map_err(|error| format!("invalid decimal value: {error}")),
            ColumnType::Date => chrono::NaiveDate::parse_from_str(literal, "%Y-%m-%d")
                .map(|value| Value::ChronoDate(Some(value)))
                .map_err(|error| format!("invalid date value: {error}")),
            ColumnType::Time => chrono::NaiveTime::parse_from_str(literal, "%H:%M:%S%.f")
                .map(|value| Value::ChronoTime(Some(value)))
                .map_err(|error| format!("invalid time value: {error}")),
            ColumnType::DateTime | ColumnType::Timestamp => {
                parse_naive_datetime(literal).map(|value| Value::ChronoDateTime(Some(value)))
            }
            ColumnType::TimestampWithTimeZone => chrono::DateTime::parse_from_rfc3339(literal)
                .map(|value| Value::ChronoDateTimeWithTimeZone(Some(value)))
                .map_err(|error| format!("invalid timestamp value: {error}")),
            ColumnType::Boolean => parse(literal).map(Value::Bool),
            ColumnType::Uuid => sea_orm::prelude::Uuid::parse_str(literal)
                .map(|value| Value::Uuid(Some(value)))
                .map_err(|error| format!("invalid UUID: {error}")),
            ColumnType::Json | ColumnType::JsonBinary => serde_json::from_str(literal)
                .map(|value| Value::Json(Some(Box::new(value))))
                .map_err(|error| format!("invalid JSON value: {error}")),
            other => Err(format!("unsupported SeaORM column type {other:?}")),
        }
    }

    fn encode_json(model_field: &str, value: &JsonValue) -> Result<Value, String> {
        let column_type = Self::column_type(model_field)?;
        if value.is_null() {
            return null_value(&column_type);
        }
        match column_type {
            ColumnType::Char(_) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| "expected a one-character JSON string".to_owned())?;
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(character), None) => Ok(Value::Char(Some(character))),
                    _ => Err("expected a one-character JSON string".to_owned()),
                }
            }
            ColumnType::String(_) | ColumnType::Text => value
                .as_str()
                .map(|text| Value::String(Some(text.to_owned())))
                .ok_or_else(|| "expected a JSON string".to_owned()),
            ColumnType::TinyInteger => signed_number::<i8>(value).map(Value::TinyInt),
            ColumnType::SmallInteger => signed_number::<i16>(value).map(Value::SmallInt),
            ColumnType::Integer => signed_number::<i32>(value).map(Value::Int),
            ColumnType::BigInteger => signed_number::<i64>(value).map(Value::BigInt),
            ColumnType::TinyUnsigned => unsigned_number::<u8>(value).map(Value::TinyUnsigned),
            ColumnType::SmallUnsigned => unsigned_number::<u16>(value).map(Value::SmallUnsigned),
            ColumnType::Unsigned => unsigned_number::<u32>(value).map(Value::Unsigned),
            ColumnType::BigUnsigned => unsigned_number::<u64>(value).map(Value::BigUnsigned),
            ColumnType::Float => float_number(value)
                .and_then(|number| {
                    let number = number as f32;
                    number.is_finite().then_some(number).ok_or_else(|| {
                        "number is outside the supported finite float range".to_owned()
                    })
                })
                .map(Some)
                .map(Value::Float),
            ColumnType::Double => float_number(value).map(Some).map(Value::Double),
            ColumnType::Decimal(_) => value
                .as_number()
                .ok_or_else(|| "expected a JSON number".to_owned())?
                .to_string()
                .parse::<rust_decimal::Decimal>()
                .map(|number| Value::Decimal(Some(number)))
                .map_err(|error| format!("invalid decimal value: {error}")),
            ColumnType::Date => value
                .as_str()
                .ok_or_else(|| "expected a date string".to_owned())
                .and_then(|text| {
                    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                        .map(|value| Value::ChronoDate(Some(value)))
                        .map_err(|error| format!("invalid date value: {error}"))
                }),
            ColumnType::Time => value
                .as_str()
                .ok_or_else(|| "expected a time string".to_owned())
                .and_then(|text| {
                    chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f")
                        .map(|value| Value::ChronoTime(Some(value)))
                        .map_err(|error| format!("invalid time value: {error}"))
                }),
            ColumnType::DateTime | ColumnType::Timestamp => value
                .as_str()
                .ok_or_else(|| "expected a date-time string".to_owned())
                .and_then(parse_naive_datetime)
                .map(|value| Value::ChronoDateTime(Some(value))),
            ColumnType::TimestampWithTimeZone => value
                .as_str()
                .ok_or_else(|| "expected an RFC 3339 timestamp string".to_owned())
                .and_then(|text| {
                    chrono::DateTime::parse_from_rfc3339(text)
                        .map(|value| Value::ChronoDateTimeWithTimeZone(Some(value)))
                        .map_err(|error| format!("invalid timestamp value: {error}"))
                }),
            ColumnType::Boolean => value
                .as_bool()
                .map(|value| Value::Bool(Some(value)))
                .ok_or_else(|| "expected a JSON boolean".to_owned()),
            ColumnType::Uuid => value
                .as_str()
                .ok_or_else(|| "expected a UUID string".to_owned())
                .and_then(|text| {
                    sea_orm::prelude::Uuid::parse_str(text)
                        .map(|value| Value::Uuid(Some(value)))
                        .map_err(|error| format!("invalid UUID: {error}"))
                }),
            ColumnType::Json | ColumnType::JsonBinary => {
                Ok(Value::Json(Some(Box::new(value.clone()))))
            }
            other => Err(format!("unsupported SeaORM column type {other:?}")),
        }
    }
}

impl<E> SeaOrmFilterValueCodec for SeaOrmColumnValueCodec<E>
where
    E: EntityTrait + Send + Sync,
    E::Column: FromStr + ColumnTrait,
{
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        Self::encode_literal(model_field, value)
    }

    fn encode_resource_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        Self::encode_literal(model_field, value)
    }
}

/// A fallible SeaORM model-to-adapter mapping contract.
pub trait SeaOrmModelMapper<E>: Send + Sync
where
    E: EntityTrait,
{
    /// Maps one loaded model to its internal adapter representation.
    fn map(&self, model: &E::Model) -> Result<AdapterResource, String>;
}

impl<E, F> SeaOrmModelMapper<E> for F
where
    E: EntityTrait,
    F: Fn(&E::Model) -> AdapterResource + Send + Sync,
{
    fn map(&self, model: &E::Model) -> Result<AdapterResource, String> {
        Ok(self(model))
    }
}

/// Wraps a fallible mapping function for use with a query executor.
pub struct FallibleSeaOrmModelMapper<F>(F);

impl<F> FallibleSeaOrmModelMapper<F> {
    /// Wraps a fallible model mapper.
    #[must_use]
    pub fn new(mapper: F) -> Self {
        Self(mapper)
    }
}

impl<E, F> SeaOrmModelMapper<E> for FallibleSeaOrmModelMapper<F>
where
    E: EntityTrait,
    F: Fn(&E::Model) -> Result<AdapterResource, String> + Send + Sync,
{
    fn map(&self, model: &E::Model) -> Result<AdapterResource, String> {
        (self.0)(model)
    }
}

impl<E> SeaOrmModelMapper<E> for FallibleSeaOrmModelMapper<SeaOrmResourceMapper<E::Model>>
where
    E: EntityTrait,
{
    fn map(&self, model: &E::Model) -> Result<AdapterResource, String> {
        (self.0.as_ref())(model)
    }
}

impl<E> SeaOrmMutationValueCodec for SeaOrmColumnValueCodec<E>
where
    E: EntityTrait + Send + Sync,
    E::Column: FromStr + ColumnTrait,
{
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        Self::encode_json(model_field, value)
    }

    fn encode_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        Self::encode_literal(model_field, value)
    }

    fn decode_identifier(&self, _model_field: &str, value: &Value) -> Result<String, String> {
        let json = value_to_json(value)?;
        match json {
            JsonValue::String(value) => Ok(value),
            JsonValue::Number(value) => Ok(value.to_string()),
            JsonValue::Null => Err("database identifier must not be null".to_owned()),
            _ => Err("database identifier must be a scalar string or number".to_owned()),
        }
    }
}

fn parse<T>(value: &str) -> Result<Option<T>, String>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse()
        .map(Some)
        .map_err(|error| format!("invalid scalar value: {error}"))
}

fn parse_naive_datetime(value: &str) -> Result<chrono::NaiveDateTime, String> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .map_err(|error| format!("invalid date-time value: {error}"))
}

fn signed_number<T>(value: &JsonValue) -> Result<Option<T>, String>
where
    T: TryFrom<i64>,
{
    let value = value
        .as_i64()
        .ok_or_else(|| "expected a signed JSON integer".to_owned())?;
    T::try_from(value)
        .map(Some)
        .map_err(|_| "integer is outside the mapped column range".to_owned())
}

fn unsigned_number<T>(value: &JsonValue) -> Result<Option<T>, String>
where
    T: TryFrom<u64>,
{
    let value = value
        .as_u64()
        .ok_or_else(|| "expected an unsigned JSON integer".to_owned())?;
    T::try_from(value)
        .map(Some)
        .map_err(|_| "integer is outside the mapped column range".to_owned())
}

fn float_number(value: &JsonValue) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| "expected a finite JSON number".to_owned())
}

fn null_value(column_type: &ColumnType) -> Result<Value, String> {
    Ok(match column_type {
        ColumnType::Char(_) => Value::Char(None),
        ColumnType::String(_) | ColumnType::Text => Value::String(None),
        ColumnType::TinyInteger => Value::TinyInt(None),
        ColumnType::SmallInteger => Value::SmallInt(None),
        ColumnType::Integer => Value::Int(None),
        ColumnType::BigInteger => Value::BigInt(None),
        ColumnType::TinyUnsigned => Value::TinyUnsigned(None),
        ColumnType::SmallUnsigned => Value::SmallUnsigned(None),
        ColumnType::Unsigned => Value::Unsigned(None),
        ColumnType::BigUnsigned => Value::BigUnsigned(None),
        ColumnType::Float => Value::Float(None),
        ColumnType::Double => Value::Double(None),
        ColumnType::Decimal(_) => Value::Decimal(None),
        ColumnType::Date => Value::ChronoDate(None),
        ColumnType::Time => Value::ChronoTime(None),
        ColumnType::DateTime | ColumnType::Timestamp => Value::ChronoDateTime(None),
        ColumnType::TimestampWithTimeZone => Value::ChronoDateTimeWithTimeZone(None),
        ColumnType::Boolean => Value::Bool(None),
        ColumnType::Uuid => Value::Uuid(None),
        ColumnType::Json | ColumnType::JsonBinary => Value::Json(None),
        other => return Err(format!("unsupported SeaORM column type {other:?}")),
    })
}

fn value_to_json(value: &Value) -> Result<JsonValue, String> {
    Ok(match value {
        Value::Bool(Some(value)) => JsonValue::Bool(*value),
        Value::Bool(None) => JsonValue::Null,
        Value::TinyInt(Some(value)) => JsonValue::from(*value),
        Value::TinyInt(None) => JsonValue::Null,
        Value::SmallInt(Some(value)) => JsonValue::from(*value),
        Value::SmallInt(None) => JsonValue::Null,
        Value::Int(Some(value)) => JsonValue::from(*value),
        Value::Int(None) => JsonValue::Null,
        Value::BigInt(Some(value)) => JsonValue::from(*value),
        Value::BigInt(None) => JsonValue::Null,
        Value::TinyUnsigned(Some(value)) => JsonValue::from(*value),
        Value::TinyUnsigned(None) => JsonValue::Null,
        Value::SmallUnsigned(Some(value)) => JsonValue::from(*value),
        Value::SmallUnsigned(None) => JsonValue::Null,
        Value::Unsigned(Some(value)) => JsonValue::from(*value),
        Value::Unsigned(None) => JsonValue::Null,
        Value::BigUnsigned(Some(value)) => JsonValue::from(*value),
        Value::BigUnsigned(None) => JsonValue::Null,
        Value::Float(Some(value)) => serde_json::Number::from_f64(f64::from(*value))
            .map(JsonValue::Number)
            .ok_or_else(|| "database returned a non-finite float".to_owned())?,
        Value::Float(None) => JsonValue::Null,
        Value::Double(Some(value)) => serde_json::Number::from_f64(*value)
            .map(JsonValue::Number)
            .ok_or_else(|| "database returned a non-finite float".to_owned())?,
        Value::Double(None) => JsonValue::Null,
        Value::Decimal(Some(value)) => JsonValue::Number(
            value
                .to_string()
                .parse()
                .map_err(|error| format!("invalid decimal JSON number: {error}"))?,
        ),
        Value::Decimal(None) => JsonValue::Null,
        Value::String(Some(value)) => JsonValue::String(value.as_str().to_owned()),
        Value::String(None) => JsonValue::Null,
        Value::Char(Some(value)) => JsonValue::String(value.to_string()),
        Value::Char(None) => JsonValue::Null,
        Value::Uuid(Some(value)) => JsonValue::String(value.to_string()),
        Value::Uuid(None) => JsonValue::Null,
        Value::ChronoDate(Some(value)) => JsonValue::String(value.to_string()),
        Value::ChronoDate(None) => JsonValue::Null,
        Value::ChronoTime(Some(value)) => JsonValue::String(value.to_string()),
        Value::ChronoTime(None) => JsonValue::Null,
        Value::ChronoDateTime(Some(value)) => JsonValue::String(value.to_string()),
        Value::ChronoDateTime(None) => JsonValue::Null,
        Value::ChronoDateTimeWithTimeZone(Some(value)) => JsonValue::String(value.to_rfc3339()),
        Value::ChronoDateTimeWithTimeZone(None) => JsonValue::Null,
        Value::Json(Some(value)) => value.as_ref().clone(),
        Value::Json(None) => JsonValue::Null,
        _ => return Err("unsupported SeaORM value type for JSON:API projection".to_owned()),
    })
}

/// Creates a public field mapping from a typed SeaORM column.
#[must_use]
pub fn attribute_mapping<E>(public_name: impl Into<String>, column: E::Column) -> AttributeMapping
where
    E: EntityTrait,
    E::Column: ColumnTrait,
{
    AttributeMapping::new(public_name, column.as_str())
}

/// A computed, read-only public attribute backed by an application mapping
/// function.
pub struct SeaOrmComputedAttribute<E>
where
    E: EntityTrait,
{
    mapping: AttributeMapping,
    mapper: SeaOrmComputedValueMapper<E::Model>,
    entity: PhantomData<fn() -> E>,
}

impl<E> Clone for SeaOrmComputedAttribute<E>
where
    E: EntityTrait,
{
    fn clone(&self) -> Self {
        Self {
            mapping: self.mapping.clone(),
            mapper: self.mapper.clone(),
            entity: PhantomData,
        }
    }
}

impl<E> SeaOrmComputedAttribute<E>
where
    E: EntityTrait,
{
    /// Returns the explicit registry mapping associated with this value.
    #[must_use]
    pub fn mapping(&self) -> &AttributeMapping {
        &self.mapping
    }
}

/// Binds a registered public attribute mapping to a computed model value.
///
/// The mapping must be registered on the resource with read permission only.
/// Computed values cannot be filtered, sorted, or written because they do not
/// correspond to a database column.
#[must_use]
pub fn computed_attribute_mapping<E, F>(
    mapping: AttributeMapping,
    mapper: F,
) -> SeaOrmComputedAttribute<E>
where
    E: EntityTrait,
    F: Fn(&E::Model) -> Result<JsonValue, String> + Send + Sync + 'static,
{
    SeaOrmComputedAttribute {
        mapping,
        mapper: Arc::new(mapper),
        entity: PhantomData,
    }
}

pub(crate) fn validate_computed_attributes<E>(
    definition: &ResourceDefinition,
    computed: &[SeaOrmComputedAttribute<E>],
) -> Result<BTreeSet<String>, String>
where
    E: EntityTrait,
{
    let mut fields = BTreeSet::new();
    for attribute in computed {
        let mapping = attribute.mapping();
        if !definition.attributes().contains(mapping) {
            return Err(format!(
                "computed attribute `{}` does not match a registered mapping on `{}`",
                mapping.public_name(),
                definition.type_name(),
            ));
        }
        if !mapping.allows(AttributePermission::Read)
            || mapping.allows(AttributePermission::Filter)
            || mapping.allows(AttributePermission::Sort)
            || mapping.allows(AttributePermission::Create)
            || mapping.allows(AttributePermission::Update)
            || mapping.allows(AttributePermission::AtomicCreate)
            || mapping.allows(AttributePermission::AtomicUpdate)
        {
            return Err(format!(
                "computed attribute `{}` must be read-only and non-queryable",
                mapping.public_name(),
            ));
        }
        if !fields.insert(mapping.model_field().to_owned()) {
            return Err(format!(
                "computed attribute field `{}` is registered more than once",
                mapping.model_field(),
            ));
        }
    }
    Ok(fields)
}

pub(crate) fn map_registered_model_with_computed<E>(
    model: &E::Model,
    definition: &ResourceDefinition,
    computed: &[SeaOrmComputedAttribute<E>],
) -> Result<AdapterResource, String>
where
    E: EntityTrait,
    E::Column: FromStr + ColumnTrait,
    E::Model: ModelTrait<Entity = E>,
{
    let fields = validate_computed_attributes::<E>(definition, computed)?;
    let mut resource = map_registered_model_skipping::<E>(model, definition, &fields)?;
    for attribute in computed {
        resource.attributes.insert(
            attribute.mapping.model_field().to_owned(),
            (attribute.mapper)(model)?,
        );
    }
    Ok(resource)
}

/// Creates a relationship mapping from a typed SeaORM foreign-key column.
#[must_use]
pub fn relationship_mapping<E>(
    public_name: impl Into<String>,
    foreign_key: E::Column,
    target_type: impl Into<String>,
    nullable: bool,
) -> RelationshipMapping
where
    E: EntityTrait,
    E::Column: ColumnTrait,
{
    RelationshipMapping::new(public_name, foreign_key.as_str(), target_type)
        .to_one_foreign_key(nullable)
}

/// Creates a to-many foreign-key mapping using the target entity's typed
/// foreign-key column.
#[must_use]
pub fn to_many_foreign_key_mapping<E>(
    public_name: impl Into<String>,
    model_field: impl Into<String>,
    target_type: impl Into<String>,
    foreign_key: E::Column,
    nullable: bool,
    reassignment: RelationshipReassignment,
) -> RelationshipMapping
where
    E: EntityTrait,
    E::Column: ColumnTrait,
{
    RelationshipMapping::new(public_name, model_field, target_type).to_many_foreign_key(
        foreign_key.as_str(),
        nullable,
        reassignment,
    )
}

/// Creates a join-table relationship mapping from the join entity's typed
/// source and target columns.
#[must_use]
pub fn join_table_relationship_mapping<E>(
    public_name: impl Into<String>,
    model_field: impl Into<String>,
    target_type: impl Into<String>,
    source_column: E::Column,
    target_column: E::Column,
) -> RelationshipMapping
where
    E: EntityTrait,
    E::Column: ColumnTrait,
{
    RelationshipMapping::new(public_name, model_field, target_type)
        .to_many_join_table(source_column.as_str(), target_column.as_str())
}

/// An included resource returned by an application-specific relationship loader.
#[derive(Clone, Debug, PartialEq)]
pub struct IncludedResource {
    /// The public JSON:API resource type.
    pub resource_type: String,
    /// The mapped resource record.
    pub resource: AdapterResource,
}

/// Loads included resources using application-specific SeaORM relations.
///
/// Relationship loading depends on each application's SeaORM entity relations
/// and authorization rules, so it is an explicit hook rather than inferred
/// from opaque registry field names. The executor calls it only after the root
/// query has succeeded and passes the validated include tree and fieldsets.
/// When configured, it also passes a per-request runtime budget for include
/// queries and related resources.
#[async_trait]
pub trait SeaOrmIncludeLoader<E>: Send + Sync
where
    E: EntityTrait,
{
    /// Loads resources requested by the validated include tree.
    ///
    /// When `runtime_budget` is present, consume the query budget before each
    /// database query and the row budget as related resources are accepted.
    /// Propagate budget errors so the router can reject excessive work before
    /// the complete result is materialized.
    ///
    /// # Errors
    ///
    /// Returns an application-level error if relation loading fails.
    async fn load_included(
        &self,
        database: &DatabaseConnection,
        roots: &[E::Model],
        root_resources: &mut [AdapterResource],
        includes: &[IncludeNode],
        fieldsets: &std::collections::BTreeMap<String, Vec<PlannedField>>,
        runtime_budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<IncludedResource>, String>;
}

#[async_trait]
pub(crate) trait StandardIncludeLoader: Send + Sync {
    async fn load_standard_included(
        &self,
        resource_type: &str,
        roots: &mut [AdapterResource],
        includes: &[IncludeNode],
        fieldsets: &BTreeMap<String, Vec<PlannedField>>,
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<IncludedResource>, String>;
}

/// A per-request budget that include loaders can consume during expansion.
///
/// Query adapters return a limit error when a configured resource or query
/// budget is exceeded. Call `consume_related_queries` before each database
/// query and `consume_related_resources` as related rows are accepted.
pub struct SeaOrmRuntimeBudget {
    maximum_related_resources: Option<usize>,
    maximum_related_queries: Option<usize>,
    related_resources: AtomicUsize,
    related_queries: AtomicUsize,
    exceeded: AtomicBool,
}

impl SeaOrmRuntimeBudget {
    /// Creates a request budget with optional related-resource and query caps.
    #[must_use]
    pub fn new(
        maximum_related_resources: Option<usize>,
        maximum_related_queries: Option<usize>,
    ) -> Self {
        Self {
            maximum_related_resources,
            maximum_related_queries,
            related_resources: AtomicUsize::new(0),
            related_queries: AtomicUsize::new(0),
            exceeded: AtomicBool::new(false),
        }
    }

    /// Charges related rows against the configured include budget.
    ///
    /// # Errors
    ///
    /// Returns an error if this charge would exceed the configured maximum.
    pub fn consume_related_resources(&self, count: usize) -> Result<(), String> {
        let result = consume_budget(
            &self.related_resources,
            count,
            self.maximum_related_resources,
            "included resources",
        );
        if result.is_err() {
            self.exceeded.store(true, AtomicOrdering::Relaxed);
        }
        result
    }

    fn related_resource_query_limit(&self) -> Option<u64> {
        self.maximum_related_resources.map(|maximum| {
            let used = self.related_resources.load(AtomicOrdering::Relaxed);
            u64::try_from(maximum.saturating_sub(used).saturating_add(1)).unwrap_or(u64::MAX)
        })
    }

    /// Charges a database query against the configured include budget.
    ///
    /// # Errors
    ///
    /// Returns an error if this charge would exceed the configured maximum.
    pub fn consume_related_queries(&self, count: usize) -> Result<(), String> {
        let result = consume_budget(
            &self.related_queries,
            count,
            self.maximum_related_queries,
            "include queries",
        );
        if result.is_err() {
            self.exceeded.store(true, AtomicOrdering::Relaxed);
        }
        result
    }

    fn was_exceeded(&self) -> bool {
        self.exceeded.load(AtomicOrdering::Relaxed)
    }
}

fn consume_budget(
    counter: &AtomicUsize,
    count: usize,
    maximum: Option<usize>,
    name: &str,
) -> Result<(), String> {
    let result = counter.fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |used| {
        let next = used.checked_add(count)?;
        maximum.map_or(Some(next), |maximum| (next <= maximum).then_some(next))
    });
    result.map(|_| ()).map_err(|used| {
        let attempted = used.saturating_add(count);
        maximum.map_or_else(
            || format!("{name} runtime budget overflowed"),
            |maximum| {
                format!("{name} count {attempted} exceeds the configured maximum of {maximum}")
            },
        )
    })
}

/// Authorizes a planned read and applies application-specific execution limits.
///
/// The executor validates the plan and applies these configured limits before
/// authorization or constructing a database query.
#[async_trait]
pub trait SeaOrmReadGuard: Send + Sync {
    /// Returns whether the caller may execute this plan.
    async fn authorize(&self, plan: &ReadPlan) -> bool;

    /// Checks application-specific query limits such as maximum page size,
    /// maximum offset, include depth, or include count.
    ///
    /// # Errors
    ///
    /// Returns a description when the plan exceeds an application limit.
    fn validate_limits(&self, _plan: &ReadPlan) -> Result<(), String> {
        Ok(())
    }
}

/// An explicit pass-through guard for query executors whose request-level
/// authorization is enforced by the HTTP `RequestAuthorizer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllSeaOrmReadGuard;

#[async_trait]
impl SeaOrmReadGuard for AllowAllSeaOrmReadGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        true
    }
}

/// The projected result of a SeaORM collection read.
#[derive(Clone, Debug, PartialEq)]
pub struct SeaOrmReadResult {
    /// Root resources after applying the requested sparse fieldset.
    pub resources: Vec<AdapterResource>,
    /// Included resources after applying their sparse fieldsets.
    pub included: Vec<IncludedResource>,
}

/// The projected result of a SeaORM single-resource read.
#[derive(Clone, Debug, PartialEq)]
pub struct SeaOrmResourceReadResult {
    /// The root resource after applying its sparse fieldset.
    pub resource: AdapterResource,
    /// Included resources after applying their sparse fieldsets.
    pub included: Vec<IncludedResource>,
}

pub(crate) struct SeaOrmResourceReadOptions<'a, E>
where
    E: EntityTrait,
{
    database: &'a DatabaseConnection,
    id: &'a str,
    plan: &'a ReadPlan,
    guard: &'a dyn SeaOrmReadGuard,
    include_loader: Option<&'a dyn SeaOrmIncludeLoader<E>>,
    runtime_budget: Option<&'a SeaOrmRuntimeBudget>,
    standard_include_loader: Option<&'a dyn StandardIncludeLoader>,
}

/// A failure while validating or executing a SeaORM read plan.
#[derive(Debug)]
pub enum SeaOrmExecutionError {
    /// A requested public resource type is absent from the supplied registry.
    UnknownResourceType(String),
    /// The read plan belongs to a different public resource type.
    ResourceTypeMismatch {
        /// The executor's registered resource type.
        expected: String,
        /// The plan's public resource type.
        actual: String,
    },
    /// A sparse fieldset contains a field or mapping not declared in the registry.
    InvalidFieldsetField {
        /// The public resource type selected by the fieldset.
        resource_type: String,
        /// The public field name supplied by the plan.
        public_name: String,
    },
    /// A filter expression uses a field not explicitly enabled for filtering.
    InvalidFilterField {
        /// The public resource type being queried.
        resource_type: String,
        /// The internal field supplied by the plan.
        model_field: String,
    },
    /// A sort term does not match a registered, explicitly sortable attribute.
    InvalidSortField {
        /// The public resource type being queried.
        resource_type: String,
        /// The public field name supplied by the plan.
        public_name: String,
    },
    /// An include node does not match a registered relationship mapping.
    InvalidIncludeRelationship {
        /// The public resource type owning the relationship.
        resource_type: String,
        /// The public relationship name supplied by the plan.
        public_name: String,
    },
    /// The page values in a manually constructed read plan are inconsistent.
    InvalidPagePlan(&'static str),
    /// An internal field name could not be resolved to an entity column.
    UnknownModelField(String),
    /// A standard row mapper cannot derive a configured relationship from a
    /// scalar entity column.
    CustomRelationshipMapperRequired(String),
    /// An include plan requires an application-specific loader.
    IncludeLoaderRequired,
    /// The read guard denied the request.
    NotAuthorized,
    /// The read guard rejected the plan for exceeding an application limit.
    LimitExceeded(String),
    /// A filter literal could not be converted to the mapped column's value type.
    InvalidFilterValue {
        /// The internal model field.
        model_field: String,
        /// The literal supplied by the client.
        value: String,
        /// The mapper's concise conversion error.
        message: String,
    },
    /// A resource identifier could not be converted to the mapped column type.
    InvalidResourceIdentifier {
        /// The internal identifier field.
        model_field: String,
        /// The identifier supplied by the client.
        value: String,
        /// The codec's concise conversion error.
        message: String,
    },
    /// A single-resource plan contains collection-only filtering or sorting.
    CollectionQueryInResourcePlan,
    /// The include loader rejected or failed the request.
    IncludeLoader(String),
    /// The database operation failed.
    Database(DbErr),
    /// A registered model mapper could not produce an adapter resource.
    ModelMapping(String),
}

impl fmt::Display for SeaOrmExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResourceType(resource_type) => {
                write!(
                    formatter,
                    "resource type `{resource_type}` is not registered"
                )
            }
            Self::ResourceTypeMismatch { expected, actual } => write!(
                formatter,
                "read plan type `{actual}` does not match executor type `{expected}`"
            ),
            Self::InvalidFieldsetField {
                resource_type,
                public_name,
            } => write!(
                formatter,
                "field `{public_name}` is not a valid registered field on resource `{resource_type}`"
            ),
            Self::InvalidFilterField {
                resource_type,
                model_field,
            } => write!(
                formatter,
                "filter field `{model_field}` is not enabled for resource `{resource_type}`"
            ),
            Self::InvalidSortField {
                resource_type,
                public_name,
            } => write!(
                formatter,
                "sort field `{public_name}` is not enabled for resource `{resource_type}`"
            ),
            Self::InvalidIncludeRelationship {
                resource_type,
                public_name,
            } => write!(
                formatter,
                "include relationship `{public_name}` is not registered on resource `{resource_type}`"
            ),
            Self::InvalidPagePlan(detail) => write!(formatter, "invalid pagination plan: {detail}"),
            Self::UnknownModelField(field) => {
                write!(formatter, "model field `{field}` is not a SeaORM column")
            }
            Self::CustomRelationshipMapperRequired(relationship) => write!(
                formatter,
                "relationship `{relationship}` requires a custom SeaORM model mapper"
            ),
            Self::IncludeLoaderRequired => {
                formatter.write_str("an include loader is required for this read plan")
            }
            Self::NotAuthorized => formatter.write_str("read plan is not authorized"),
            Self::LimitExceeded(message) => {
                write!(formatter, "read plan exceeds configured limits: {message}")
            }
            Self::InvalidFilterValue {
                model_field,
                value,
                message,
            } => write!(
                formatter,
                "filter value `{value}` is invalid for model field `{model_field}`: {message}"
            ),
            Self::InvalidResourceIdentifier {
                model_field,
                value,
                message,
            } => write!(
                formatter,
                "resource identifier `{value}` is invalid for model field `{model_field}`: {message}"
            ),
            Self::CollectionQueryInResourcePlan => formatter
                .write_str("single-resource reads do not support filters, sorting, or pagination"),
            Self::IncludeLoader(message) => write!(formatter, "include loading failed: {message}"),
            Self::Database(error) => write!(formatter, "database read failed: {error}"),
            Self::ModelMapping(message) => write!(formatter, "model mapping failed: {message}"),
        }
    }
}

impl std::error::Error for SeaOrmExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Executes one registered resource's validated collection and resource plans.
///
/// The mapper converts typed SeaORM models into adapter records. Fieldset
/// projection is enforced by the executor after mapping, so undeclared model
/// values cannot escape merely because a mapper returned them.
pub struct SeaOrmQueryExecutor<E, M, C>
where
    E: EntityTrait,
    M: SeaOrmModelMapper<E>,
    C: SeaOrmFilterValueCodec,
{
    registry: ResourceRegistry,
    resource_type: String,
    mapper: M,
    filter_value_codec: C,
    entity: PhantomData<fn() -> E>,
}

impl<E, M, C> SeaOrmQueryExecutor<E, M, C>
where
    E: EntityTrait,
    E::Column: FromStr,
    M: SeaOrmModelMapper<E>,
    C: SeaOrmFilterValueCodec,
{
    /// Creates an executor for a registered public resource type.
    ///
    /// Validates the identifier column and every explicitly filterable or
    /// sortable attribute against the SeaORM entity before the executor can
    /// be used. Attribute mappers may still expose computed, non-queryable
    /// fields, and relationship mapping remains application-defined.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource is not registered or one of its
    /// required entity-column mappings does not exist.
    pub fn new(
        registry: ResourceRegistry,
        resource_type: impl Into<String>,
        mapper: M,
        filter_value_codec: C,
    ) -> Result<Self, SeaOrmExecutionError> {
        let resource_type = resource_type.into();
        let definition = registry
            .resource(&resource_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(resource_type.clone()))?;
        column::<E>(definition.identifier_field())?;
        for attribute in definition
            .attributes()
            .iter()
            .filter(|attribute| attribute.is_filterable() || attribute.is_sortable())
        {
            column::<E>(attribute.model_field())?;
        }
        Ok(Self {
            registry,
            resource_type,
            mapper,
            filter_value_codec,
            entity: PhantomData,
        })
    }

    /// Executes the root query in the configured database backend and projects
    /// its mapped results.
    ///
    /// Filters, sort order, offset, and limit are translated to SeaORM
    /// expressions and run by the database. If includes are requested, the
    /// supplied loader is called with the root rows, include tree, and
    /// fieldsets. The required read guard authorizes and checks application
    /// limits before a database query is constructed. No in-memory filtering
    /// or sorting fallback is used.
    ///
    /// # Errors
    ///
    /// Returns an error before querying for a resource mismatch, invalid
    /// entity-column mapping, or a missing include loader. Database and
    /// application-specific include failures are returned explicitly.
    pub async fn collection(
        &self,
        database: &DatabaseConnection,
        plan: &ReadPlan,
        guard: &dyn SeaOrmReadGuard,
        include_loader: Option<&dyn SeaOrmIncludeLoader<E>>,
    ) -> Result<SeaOrmReadResult, SeaOrmExecutionError>
    where
        E::Column: ColumnTrait,
    {
        self.collection_with_runtime_budget(database, plan, guard, include_loader, None, None)
            .await
    }

    pub(crate) async fn collection_with_runtime_budget(
        &self,
        database: &DatabaseConnection,
        plan: &ReadPlan,
        guard: &dyn SeaOrmReadGuard,
        include_loader: Option<&dyn SeaOrmIncludeLoader<E>>,
        runtime_budget: Option<&SeaOrmRuntimeBudget>,
        standard_include_loader: Option<&dyn StandardIncludeLoader>,
    ) -> Result<SeaOrmReadResult, SeaOrmExecutionError>
    where
        E::Column: ColumnTrait,
    {
        let definition = self
            .registry
            .resource(&self.resource_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(self.resource_type.clone()))?;
        if plan.resource_type != self.resource_type {
            return Err(SeaOrmExecutionError::ResourceTypeMismatch {
                expected: self.resource_type.clone(),
                actual: plan.resource_type.clone(),
            });
        }
        validate_page_plan(plan.page)?;
        validate_fieldset_mappings(&self.registry, plan)?;
        validate_include_mappings(&self.registry, &plan.resource_type, &plan.includes)?;
        let include_relationships = include_relationships_by_type(plan);
        validate_filter_mappings(definition, plan)?;
        validate_sort_mappings(definition, plan)?;
        guard
            .validate_limits(plan)
            .map_err(SeaOrmExecutionError::LimitExceeded)?;
        if !guard.authorize(plan).await {
            return Err(SeaOrmExecutionError::NotAuthorized);
        }
        let relationship_linkage_requested = definition.relationships().iter().any(|mapping| {
            mapping.allows(crate::registry::RelationshipPermission::Read)
                && matches!(
                    mapping.storage(),
                    RelationshipStorage::ToManyForeignKey { .. }
                        | RelationshipStorage::JoinTable { .. }
                )
        });
        if (!plan.includes.is_empty() || relationship_linkage_requested)
            && include_loader.is_none()
            && standard_include_loader.is_none()
        {
            return Err(SeaOrmExecutionError::IncludeLoaderRequired);
        }

        let mut select = E::find();
        if let Some(filter) = &plan.filter {
            select = select.filter(filter_condition::<E, C>(filter, &self.filter_value_codec)?);
        }
        for sort in &plan.sort {
            let column = column::<E>(&sort.model_field)?;
            select = select.order_by(column.is_null(), Order::Asc);
            select = select.order_by(
                column,
                match sort.direction {
                    SortDirection::Ascending => Order::Asc,
                    SortDirection::Descending => Order::Desc,
                },
            );
        }
        let rows = select
            .offset(plan.page.offset)
            .limit(plan.page.limit)
            .all(database)
            .await
            .map_err(SeaOrmExecutionError::Database)?;

        let fieldset = plan.fieldsets.get(&plan.resource_type).map(Vec::as_slice);
        let mut resources = rows
            .iter()
            .map(|row| {
                self.mapper
                    .map(row)
                    .map_err(SeaOrmExecutionError::ModelMapping)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let included = if let Some(loader) =
            include_loader.filter(|_| !plan.includes.is_empty() || relationship_linkage_requested)
        {
            let result = loader
                .load_included(
                    database,
                    &rows,
                    &mut resources,
                    &plan.includes,
                    &plan.fieldsets,
                    runtime_budget,
                )
                .await;
            result
                .map_err(|message| match runtime_budget {
                    Some(budget) if budget.was_exceeded() => {
                        SeaOrmExecutionError::LimitExceeded(message)
                    }
                    _ => SeaOrmExecutionError::IncludeLoader(message),
                })?
                .into_iter()
                .map(|mut included| {
                    let definition =
                        self.registry
                            .resource(&included.resource_type)
                            .map_err(|_| {
                                SeaOrmExecutionError::UnknownResourceType(
                                    included.resource_type.clone(),
                                )
                            })?;
                    let fieldset = plan
                        .fieldsets
                        .get(&included.resource_type)
                        .map(Vec::as_slice);
                    included.resource = project_adapter_record_with_includes(
                        definition,
                        included.resource,
                        fieldset,
                        include_relationships
                            .get(&included.resource_type)
                            .unwrap_or(&BTreeSet::new()),
                    );
                    Ok(included)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else if let Some(loader) = standard_include_loader
            .filter(|_| !plan.includes.is_empty() || relationship_linkage_requested)
        {
            loader
                .load_standard_included(
                    &self.resource_type,
                    &mut resources,
                    &plan.includes,
                    &plan.fieldsets,
                    runtime_budget,
                )
                .await
                .map_err(|message| match runtime_budget {
                    Some(budget) if budget.was_exceeded() => {
                        SeaOrmExecutionError::LimitExceeded(message)
                    }
                    _ => SeaOrmExecutionError::IncludeLoader(message),
                })?
        } else {
            Vec::new()
        };

        let resources = resources
            .into_iter()
            .map(|resource| {
                project_adapter_record_with_includes(
                    definition,
                    resource,
                    fieldset,
                    include_relationships
                        .get(&self.resource_type)
                        .unwrap_or(&BTreeSet::new()),
                )
            })
            .collect();

        Ok(SeaOrmReadResult {
            resources,
            included,
        })
    }

    /// Executes a single-resource read by its public persistent identifier.
    ///
    /// Includes and sparse fieldsets use the same validated, adapter-independent
    /// plan as collection reads. The configured codec converts the identifier
    /// to the entity's typed column value before the database query.
    ///
    /// # Errors
    ///
    /// Returns an error before querying for a resource mismatch, unsupported
    /// collection-only filter/sort plan, invalid field/include mapping,
    /// identifier conversion failure, a missing include loader, authorization
    /// denial, or an application limit failure.
    pub async fn resource(
        &self,
        database: &DatabaseConnection,
        id: &str,
        plan: &ReadPlan,
        guard: &dyn SeaOrmReadGuard,
        include_loader: Option<&dyn SeaOrmIncludeLoader<E>>,
    ) -> Result<Option<SeaOrmResourceReadResult>, SeaOrmExecutionError>
    where
        E::Column: ColumnTrait,
    {
        self.resource_with_runtime_budget(SeaOrmResourceReadOptions {
            database,
            id,
            plan,
            guard,
            include_loader,
            runtime_budget: None,
            standard_include_loader: None,
        })
        .await
    }

    pub(crate) async fn resource_with_runtime_budget(
        &self,
        options: SeaOrmResourceReadOptions<'_, E>,
    ) -> Result<Option<SeaOrmResourceReadResult>, SeaOrmExecutionError>
    where
        E::Column: ColumnTrait,
    {
        let SeaOrmResourceReadOptions {
            database,
            id,
            plan,
            guard,
            include_loader,
            runtime_budget,
            standard_include_loader,
        } = options;
        let definition = self
            .registry
            .resource(&self.resource_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(self.resource_type.clone()))?;
        if plan.resource_type != self.resource_type {
            return Err(SeaOrmExecutionError::ResourceTypeMismatch {
                expected: self.resource_type.clone(),
                actual: plan.resource_type.clone(),
            });
        }
        if plan.filter.is_some()
            || !plan.sort.is_empty()
            || plan.page
                != (Page {
                    number: 1,
                    size: 1,
                    offset: 0,
                    limit: 1,
                })
        {
            return Err(SeaOrmExecutionError::CollectionQueryInResourcePlan);
        }
        validate_page_plan(plan.page)?;
        validate_fieldset_mappings(&self.registry, plan)?;
        validate_include_mappings(&self.registry, &plan.resource_type, &plan.includes)?;
        let include_relationships = include_relationships_by_type(plan);
        guard
            .validate_limits(plan)
            .map_err(SeaOrmExecutionError::LimitExceeded)?;
        if !guard.authorize(plan).await {
            return Err(SeaOrmExecutionError::NotAuthorized);
        }
        let relationship_linkage_requested = definition.relationships().iter().any(|mapping| {
            mapping.allows(crate::registry::RelationshipPermission::Read)
                && matches!(
                    mapping.storage(),
                    RelationshipStorage::ToManyForeignKey { .. }
                        | RelationshipStorage::JoinTable { .. }
                )
        });
        if (!plan.includes.is_empty() || relationship_linkage_requested)
            && include_loader.is_none()
            && standard_include_loader.is_none()
        {
            return Err(SeaOrmExecutionError::IncludeLoaderRequired);
        }

        let identifier_value = self
            .filter_value_codec
            .encode_resource_identifier(definition.identifier_field(), id)
            .map_err(|message| SeaOrmExecutionError::InvalidResourceIdentifier {
                model_field: definition.identifier_field().to_owned(),
                value: id.to_owned(),
                message,
            })?;
        let identifier_column = column::<E>(definition.identifier_field())?;
        let Some(row) = E::find()
            .filter(identifier_column.eq(identifier_value))
            .one(database)
            .await
            .map_err(SeaOrmExecutionError::Database)?
        else {
            return Ok(None);
        };

        let fieldset = plan.fieldsets.get(&plan.resource_type).map(Vec::as_slice);
        let mapped_resource = self
            .mapper
            .map(&row)
            .map_err(SeaOrmExecutionError::ModelMapping)?;
        let mut resource = mapped_resource;
        let included = if let Some(loader) =
            include_loader.filter(|_| !plan.includes.is_empty() || relationship_linkage_requested)
        {
            let roots = std::slice::from_ref(&row);
            let result = loader
                .load_included(
                    database,
                    roots,
                    std::slice::from_mut(&mut resource),
                    &plan.includes,
                    &plan.fieldsets,
                    runtime_budget,
                )
                .await;
            result
                .map_err(|message| match runtime_budget {
                    Some(budget) if budget.was_exceeded() => {
                        SeaOrmExecutionError::LimitExceeded(message)
                    }
                    _ => SeaOrmExecutionError::IncludeLoader(message),
                })?
                .into_iter()
                .map(|mut included| {
                    let definition =
                        self.registry
                            .resource(&included.resource_type)
                            .map_err(|_| {
                                SeaOrmExecutionError::UnknownResourceType(
                                    included.resource_type.clone(),
                                )
                            })?;
                    let fieldset = plan
                        .fieldsets
                        .get(&included.resource_type)
                        .map(Vec::as_slice);
                    included.resource = project_adapter_record_with_includes(
                        definition,
                        included.resource,
                        fieldset,
                        include_relationships
                            .get(&included.resource_type)
                            .unwrap_or(&BTreeSet::new()),
                    );
                    Ok(included)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else if let Some(loader) = standard_include_loader
            .filter(|_| !plan.includes.is_empty() || relationship_linkage_requested)
        {
            loader
                .load_standard_included(
                    &self.resource_type,
                    std::slice::from_mut(&mut resource),
                    &plan.includes,
                    &plan.fieldsets,
                    runtime_budget,
                )
                .await
                .map_err(|message| match runtime_budget {
                    Some(budget) if budget.was_exceeded() => {
                        SeaOrmExecutionError::LimitExceeded(message)
                    }
                    _ => SeaOrmExecutionError::IncludeLoader(message),
                })?
        } else {
            Vec::new()
        };

        let resource = project_adapter_record_with_includes(
            definition,
            resource,
            fieldset,
            include_relationships
                .get(&self.resource_type)
                .unwrap_or(&BTreeSet::new()),
        );

        Ok(Some(SeaOrmResourceReadResult { resource, included }))
    }
}

impl<E>
    SeaOrmQueryExecutor<
        E,
        FallibleSeaOrmModelMapper<
            Arc<dyn Fn(&E::Model) -> Result<AdapterResource, String> + Send + Sync>,
        >,
        SeaOrmColumnValueCodec<E>,
    >
where
    E: EntityTrait + 'static,
    E::Column: FromStr + ColumnTrait,
    E::Model: ModelTrait<Entity = E>,
{
    /// Creates an executor that maps explicitly registered entity columns to
    /// adapter records and uses the standard scalar codec.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource or identifier mapping is invalid.
    pub fn mapped(
        registry: ResourceRegistry,
        resource_type: impl Into<String>,
    ) -> Result<Self, SeaOrmExecutionError> {
        Self::mapped_with_computed(registry, resource_type, Vec::new())
    }

    /// Creates a standard mapped executor with additional computed,
    /// read-only attributes.
    ///
    /// Every selected attribute must be explicitly present in the resource
    /// registry. Computed mappings must be registered with read-only
    /// permissions and cannot be filtered or sorted.
    ///
    /// # Errors
    ///
    /// Returns an error when an attribute or relationship mapping is invalid
    /// for the entity, or when a computed mapping does not match the registry.
    pub fn mapped_with_computed(
        registry: ResourceRegistry,
        resource_type: impl Into<String>,
        computed: Vec<SeaOrmComputedAttribute<E>>,
    ) -> Result<Self, SeaOrmExecutionError> {
        let resource_type = resource_type.into();
        let definition = registry
            .resource(&resource_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(resource_type.clone()))?
            .clone();
        let computed_fields = validate_computed_attributes::<E>(&definition, &computed)
            .map_err(SeaOrmExecutionError::ModelMapping)?;
        column::<E>(definition.identifier_field())?;
        for attribute in definition.attributes() {
            if !computed_fields.contains(attribute.model_field()) {
                column::<E>(attribute.model_field())?;
            }
        }
        for relationship in definition.relationships() {
            if matches!(
                relationship.storage(),
                RelationshipStorage::ToManyForeignKey { .. }
                    | RelationshipStorage::JoinTable { .. }
            ) {
                continue;
            }
            if relationship.cardinality() == Some(crate::registry::RelationshipCardinality::ToMany)
            {
                if relationship.allows(crate::registry::RelationshipPermission::Read) {
                    return Err(SeaOrmExecutionError::CustomRelationshipMapperRequired(
                        relationship.public_name().to_owned(),
                    ));
                }
                continue;
            }
            column::<E>(relationship.model_field())?;
        }
        let mapper: SeaOrmResourceMapper<E::Model> = Arc::new(move |model| {
            map_registered_model_with_computed::<E>(model, &definition, &computed)
        });
        Self::new(
            registry,
            resource_type,
            FallibleSeaOrmModelMapper::new(mapper),
            SeaOrmColumnValueCodec::default(),
        )
    }
}

fn map_registered_model_skipping<E>(
    model: &E::Model,
    definition: &ResourceDefinition,
    skipped_attribute_fields: &BTreeSet<String>,
) -> Result<AdapterResource, String>
where
    E: EntityTrait,
    E::Column: FromStr + ColumnTrait,
    E::Model: ModelTrait<Entity = E>,
{
    let identifier_column = E::Column::from_str(definition.identifier_field()).map_err(|_| {
        format!(
            "identifier field `{}` is not a SeaORM column",
            definition.identifier_field()
        )
    })?;
    let id = scalar_identifier(&model.get(identifier_column))?;
    let mut resource = AdapterResource {
        id,
        ..AdapterResource::default()
    };
    for attribute in definition.attributes() {
        if skipped_attribute_fields.contains(attribute.model_field()) {
            continue;
        }
        let column = E::Column::from_str(attribute.model_field()).map_err(|_| {
            format!(
                "attribute `{}` maps to non-column `{}`; use a custom model mapper for computed fields",
                attribute.public_name(),
                attribute.model_field()
            )
        })?;
        resource.attributes.insert(
            attribute.model_field().to_owned(),
            value_to_json(&model.get(column))?,
        );
    }
    for relationship in definition.relationships() {
        if matches!(
            relationship.storage(),
            RelationshipStorage::ToManyForeignKey { .. } | RelationshipStorage::JoinTable { .. }
        ) {
            resource.relationships.insert(
                relationship.model_field().to_owned(),
                crate::document::Relationship {
                    data: Some(RelationshipData::Many(Vec::new())),
                    ..crate::document::Relationship::default()
                },
            );
            continue;
        }
        let Ok(column) = E::Column::from_str(relationship.model_field()) else {
            if relationship.cardinality().is_some() {
                continue;
            }
            return Err(format!(
                "relationship `{}` has no mapped foreign-key column; use a custom relationship mapper",
                relationship.public_name()
            ));
        };
        let value = value_to_json(&model.get(column))?;
        let data = match value {
            JsonValue::Null => RelationshipData::Null,
            value => RelationshipData::One(crate::document::ResourceIdentifier {
                type_name: relationship.target_type().to_owned(),
                id: Some(scalar_identifier_json(value)?),
                ..crate::document::ResourceIdentifier::default()
            }),
        };
        resource.relationships.insert(
            relationship.model_field().to_owned(),
            crate::document::Relationship {
                data: Some(data),
                ..crate::document::Relationship::default()
            },
        );
    }
    Ok(resource)
}

fn scalar_identifier(value: &Value) -> Result<String, String> {
    scalar_identifier_json(value_to_json(value)?)
}

fn scalar_identifier_json(value: JsonValue) -> Result<String, String> {
    match value {
        JsonValue::String(value) => Ok(value),
        JsonValue::Number(value) => Ok(value.to_string()),
        _ => Err("resource identifier must be a scalar string or number".to_owned()),
    }
}

#[async_trait]
trait BoundSeaOrmQueryExecutor: Send + Sync {
    fn has_custom_include_loader(&self) -> bool;
    fn validate_column(&self, field: &str) -> Result<(), String>;
    async fn collection(
        &self,
        plan: &ReadPlan,
        limits: Option<&ExecutionLimits>,
        standard_include_loader: Option<&dyn StandardIncludeLoader>,
    ) -> Result<QueryCollectionResult, QueryAdapterError>;
    async fn resource(
        &self,
        id: &str,
        plan: &ReadPlan,
        limits: Option<&ExecutionLimits>,
        standard_include_loader: Option<&dyn StandardIncludeLoader>,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError>;
    async fn resources_by_ids(
        &self,
        ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<AdapterResource>, String>;
    async fn resources_by_foreign_key(
        &self,
        field: &str,
        source_ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<(String, AdapterResource)>, String>;
}

#[async_trait]
trait BoundJoinTableQuery: Send + Sync {
    async fn targets_by_source_ids(
        &self,
        source_ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<BTreeMap<String, Vec<String>>, String>;
}

struct JoinTableQuery<E>
where
    E: EntityTrait,
{
    database: DatabaseConnection,
    source_column: E::Column,
    target_column: E::Column,
    source_column_name: String,
}

#[async_trait]
impl<E> BoundJoinTableQuery for JoinTableQuery<E>
where
    E: EntityTrait + Send + Sync,
    E::Column: ColumnTrait + FromStr + Send + Sync,
    E::Model: ModelTrait<Entity = E>,
{
    async fn targets_by_source_ids(
        &self,
        source_ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<BTreeMap<String, Vec<String>>, String> {
        if source_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let codec = SeaOrmColumnValueCodec::<E>::default();
        let values = source_ids
            .iter()
            .map(|id| codec.encode_filter_value(&self.source_column_name, id))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(budget) = budget {
            budget.consume_related_queries(1)?;
        }
        let mut select = E::find().filter(self.source_column.is_in(values));
        if let Some(limit) = budget.and_then(SeaOrmRuntimeBudget::related_resource_query_limit) {
            select = select.limit(limit);
        }
        let rows = select
            .all(&self.database)
            .await
            .map_err(|error| error.to_string())?;
        let mut targets = BTreeMap::<String, Vec<String>>::new();
        for row in rows {
            let source_id = scalar_identifier_json(value_to_json(&row.get(self.source_column))?)?;
            let target_id = scalar_identifier_json(value_to_json(&row.get(self.target_column))?)?;
            targets.entry(source_id).or_default().push(target_id);
        }
        Ok(targets)
    }
}

struct BoundQueryExecutor<E, M, C>
where
    E: EntityTrait,
    E::Column: FromStr,
    M: SeaOrmModelMapper<E>,
    C: SeaOrmFilterValueCodec,
{
    database: DatabaseConnection,
    executor: SeaOrmQueryExecutor<E, M, C>,
    guard: Arc<dyn SeaOrmReadGuard>,
    include_loader: Option<Arc<dyn SeaOrmIncludeLoader<E>>>,
}

#[async_trait]
impl<E, M, C> BoundSeaOrmQueryExecutor for BoundQueryExecutor<E, M, C>
where
    E: EntityTrait + 'static,
    E::Column: FromStr + ColumnTrait,
    M: SeaOrmModelMapper<E> + 'static,
    C: SeaOrmFilterValueCodec + 'static,
{
    fn has_custom_include_loader(&self) -> bool {
        self.include_loader.is_some()
    }

    fn validate_column(&self, field: &str) -> Result<(), String> {
        E::Column::from_str(field)
            .map(|_| ())
            .map_err(|_| format!("field `{field}` is not a SeaORM column"))
    }

    async fn collection(
        &self,
        plan: &ReadPlan,
        limits: Option<&ExecutionLimits>,
        standard_include_loader: Option<&dyn StandardIncludeLoader>,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        let result = if let Some(limits) = limits {
            let budget = limits.seaorm_runtime_budget();
            self.executor
                .collection_with_runtime_budget(
                    &self.database,
                    plan,
                    self.guard.as_ref(),
                    self.include_loader.as_deref(),
                    Some(&budget),
                    standard_include_loader,
                )
                .await
        } else {
            self.executor
                .collection_with_runtime_budget(
                    &self.database,
                    plan,
                    self.guard.as_ref(),
                    self.include_loader.as_deref(),
                    None,
                    standard_include_loader,
                )
                .await
        }
        .map_err(query_adapter_error)?;
        Ok(QueryCollectionResult {
            resources: result.resources,
            included: result
                .included
                .into_iter()
                .map(|included| AdapterIncludedResource {
                    resource_type: included.resource_type,
                    resource: included.resource,
                })
                .collect(),
        })
    }

    async fn resource(
        &self,
        id: &str,
        plan: &ReadPlan,
        limits: Option<&ExecutionLimits>,
        standard_include_loader: Option<&dyn StandardIncludeLoader>,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        let result = if let Some(limits) = limits {
            let budget = limits.seaorm_runtime_budget();
            self.executor
                .resource_with_runtime_budget(SeaOrmResourceReadOptions {
                    database: &self.database,
                    id,
                    plan,
                    guard: self.guard.as_ref(),
                    include_loader: self.include_loader.as_deref(),
                    runtime_budget: Some(&budget),
                    standard_include_loader,
                })
                .await
        } else {
            self.executor
                .resource_with_runtime_budget(SeaOrmResourceReadOptions {
                    database: &self.database,
                    id,
                    plan,
                    guard: self.guard.as_ref(),
                    include_loader: self.include_loader.as_deref(),
                    runtime_budget: None,
                    standard_include_loader,
                })
                .await
        }
        .map_err(query_adapter_error)?;
        Ok(result.map(|result| QueryResourceResult {
            resource: result.resource,
            included: result
                .included
                .into_iter()
                .map(|included| AdapterIncludedResource {
                    resource_type: included.resource_type,
                    resource: included.resource,
                })
                .collect(),
        }))
    }

    async fn resources_by_ids(
        &self,
        ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<AdapterResource>, String> {
        let identifier_field = self
            .executor
            .registry
            .resource(&self.executor.resource_type)
            .map_err(|error| error.to_string())?
            .identifier_field()
            .to_owned();
        self.query_resources_by_values(&identifier_field, ids, true, budget)
            .await
            .map(|rows| rows.into_iter().map(|(_, resource)| resource).collect())
    }

    async fn resources_by_foreign_key(
        &self,
        field: &str,
        source_ids: &[String],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<(String, AdapterResource)>, String> {
        self.query_resources_by_values(field, source_ids, false, budget)
            .await
    }
}

impl<E, M, C> BoundQueryExecutor<E, M, C>
where
    E: EntityTrait + 'static,
    E::Column: FromStr + ColumnTrait,
    E::Model: ModelTrait<Entity = E>,
    M: SeaOrmModelMapper<E> + 'static,
    C: SeaOrmFilterValueCodec + 'static,
{
    async fn query_resources_by_values(
        &self,
        field: &str,
        values: &[String],
        resource_identifier: bool,
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<(String, AdapterResource)>, String> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let field_column = E::Column::from_str(field)
            .map_err(|_| format!("related field `{field}` is not a SeaORM column"))?;
        let query_values = values
            .iter()
            .map(|value| {
                if resource_identifier {
                    self.executor
                        .filter_value_codec
                        .encode_resource_identifier(field, value)
                } else {
                    self.executor
                        .filter_value_codec
                        .encode_filter_value(field, value)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(budget) = budget {
            budget.consume_related_queries(1)?;
        }
        let mut select = E::find().filter(field_column.is_in(query_values));
        if let Some(limit) = budget.and_then(SeaOrmRuntimeBudget::related_resource_query_limit) {
            select = select.limit(limit);
        }
        let models = select
            .all(&self.database)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(budget) = budget {
            budget.consume_related_resources(models.len())?;
        }
        models
            .into_iter()
            .map(|model| {
                let group_id = scalar_identifier_json(value_to_json(&model.get(field_column))?)?;
                let resource = self.executor.mapper.map(&model)?;
                Ok((group_id, resource))
            })
            .collect()
    }
}

/// A standard query adapter that dispatches validated plans to explicitly
/// registered typed SeaORM executors. Registration rejects duplicate resource
/// handlers instead of silently choosing the first matching executor.
#[derive(Default)]
pub struct SeaOrmQueryAdapter {
    executors: BTreeMap<String, Arc<dyn BoundSeaOrmQueryExecutor>>,
    registry: Option<ResourceRegistry>,
    join_table_queries: BTreeMap<(String, String), Arc<dyn BoundJoinTableQuery>>,
}

impl SeaOrmQueryAdapter {
    /// Creates an empty query adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a typed executor, database, read guard, and optional include loader.
    ///
    /// # Errors
    ///
    /// Returns an error when another executor is already registered for the
    /// same public resource type.
    pub fn register<E, M, C>(
        &mut self,
        database: DatabaseConnection,
        executor: SeaOrmQueryExecutor<E, M, C>,
        guard: Arc<dyn SeaOrmReadGuard>,
        include_loader: Option<Arc<dyn SeaOrmIncludeLoader<E>>>,
    ) -> Result<(), SeaOrmAdapterConfigurationError>
    where
        E: EntityTrait + 'static,
        E::Column: FromStr + ColumnTrait,
        M: SeaOrmModelMapper<E> + 'static,
        C: SeaOrmFilterValueCodec + 'static,
    {
        if let Some(registry) = &self.registry
            && registry != &executor.registry
        {
            return Err(SeaOrmAdapterConfigurationError::RegistryMismatch);
        }
        if self.executors.contains_key(&executor.resource_type) {
            return Err(SeaOrmAdapterConfigurationError::DuplicateQueryExecutor(
                executor.resource_type.clone(),
            ));
        }
        self.registry = Some(executor.registry.clone());
        self.executors.insert(
            executor.resource_type.clone(),
            Arc::new(BoundQueryExecutor {
                database,
                executor,
                guard,
                include_loader,
            }),
        );
        Ok(())
    }

    /// Registers the typed join-table entity used by a declared relationship.
    ///
    /// The relationship must use [`RelationshipStorage::JoinTable`]. The
    /// configured columns are checked against the typed entity before the
    /// query adapter can be built.
    pub fn register_join_table<E>(
        &mut self,
        source_type: &str,
        relationship_name: &str,
        database: DatabaseConnection,
    ) -> Result<(), SeaOrmAdapterConfigurationError>
    where
        E: EntityTrait + Send + Sync + 'static,
        E::Column: FromStr + ColumnTrait + Send + Sync,
        E::Model: ModelTrait<Entity = E>,
    {
        let registry = self.registry.as_ref().ok_or_else(|| {
            SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(
                "register a resource query executor before its join table".to_owned(),
            )
        })?;
        let relationship = registry
            .relationship(source_type, relationship_name)
            .map_err(|error| {
                SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(error.to_string())
            })?;
        let RelationshipStorage::JoinTable {
            source_column,
            target_column,
        } = relationship.storage()
        else {
            return Err(SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(
                format!("`{source_type}.{relationship_name}` is not mapped through a join table"),
            ));
        };
        let source_column = E::Column::from_str(source_column).map_err(|_| {
            SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(format!(
                "join source column `{source_column}` is absent from the typed join entity"
            ))
        })?;
        let target_column = E::Column::from_str(target_column).map_err(|_| {
            SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(format!(
                "join target column `{target_column}` is absent from the typed join entity"
            ))
        })?;
        let key = (source_type.to_owned(), relationship_name.to_owned());
        if self.join_table_queries.contains_key(&key) {
            return Err(SeaOrmAdapterConfigurationError::InvalidRelationshipMapping(
                format!("join table for `{source_type}.{relationship_name}` is already registered"),
            ));
        }
        self.join_table_queries.insert(
            key,
            Arc::new(JoinTableQuery::<E> {
                database,
                source_column,
                target_column,
                source_column_name: match relationship.storage() {
                    RelationshipStorage::JoinTable { source_column, .. } => source_column.clone(),
                    _ => unreachable!(),
                },
            }),
        );
        Ok(())
    }

    async fn relationship_resources(
        &self,
        source_type: &str,
        relationship: &RelationshipMapping,
        sources: &[AdapterResource],
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<(String, AdapterResource)>, String> {
        let target_type = relationship.target_type();
        let target_executor = self.executors.get(target_type).ok_or_else(|| {
            format!("no SeaORM query executor is registered for related type `{target_type}`")
        })?;
        match relationship.storage() {
            RelationshipStorage::ToOneForeignKey { .. } => {
                let mut requested = Vec::new();
                for source in sources {
                    let relation = source
                        .relationships
                        .get(relationship.model_field())
                        .ok_or_else(|| {
                            format!(
                                "mapped resource `{source_type}` omitted relationship field `{}`",
                                relationship.model_field()
                            )
                        })?;
                    if let Some(crate::document::RelationshipData::One(identifier)) =
                        relation.data.as_ref()
                        && let Some(id) = &identifier.id
                    {
                        requested.push(id.clone());
                    }
                }
                let requested_set = requested.iter().cloned().collect::<BTreeSet<_>>();
                let related = target_executor
                    .resources_by_ids(&requested_set.into_iter().collect::<Vec<_>>(), budget)
                    .await?;
                let related = related
                    .into_iter()
                    .map(|resource| (resource.id.clone(), resource))
                    .collect::<BTreeMap<_, _>>();
                let mut pairs = Vec::new();
                for source in sources {
                    let Some(crate::document::RelationshipData::One(identifier)) = source
                        .relationships
                        .get(relationship.model_field())
                        .and_then(|relation| relation.data.as_ref())
                    else {
                        continue;
                    };
                    let Some(id) = identifier.id.as_ref() else {
                        continue;
                    };
                    if let Some(resource) = related.get(id) {
                        pairs.push((source.id.clone(), resource.clone()));
                    }
                }
                Ok(pairs)
            }
            RelationshipStorage::ToManyForeignKey {
                foreign_key_field, ..
            } => {
                let source_ids = sources
                    .iter()
                    .map(|source| source.id.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                target_executor
                    .resources_by_foreign_key(foreign_key_field, &source_ids, budget)
                    .await
            }
            RelationshipStorage::JoinTable { .. } => {
                let key = (
                    source_type.to_owned(),
                    relationship.public_name().to_owned(),
                );
                let join_query = self.join_table_queries.get(&key).ok_or_else(|| {
                    format!(
                        "no typed join-table entity is registered for `{source_type}.{}`",
                        relationship.public_name()
                    )
                })?;
                let source_ids = sources
                    .iter()
                    .map(|source| source.id.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let targets = join_query
                    .targets_by_source_ids(&source_ids, budget)
                    .await?;
                let target_ids = targets
                    .values()
                    .flatten()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let resources = target_executor
                    .resources_by_ids(&target_ids, budget)
                    .await?
                    .into_iter()
                    .map(|resource| (resource.id.clone(), resource))
                    .collect::<BTreeMap<_, _>>();
                let mut pairs = Vec::new();
                for (source_id, target_ids) in targets {
                    for target_id in target_ids {
                        if let Some(resource) = resources.get(&target_id) {
                            pairs.push((source_id.clone(), resource.clone()));
                        }
                    }
                }
                Ok(pairs)
            }
            RelationshipStorage::Custom => Err(format!(
                "relationship `{source_type}.{}` requires a custom include loader",
                relationship.public_name()
            )),
        }
    }

    fn expand_standard_level<'a>(
        &'a self,
        resource_type: &'a str,
        sources: &'a mut [AdapterResource],
        includes: &'a [IncludeNode],
        fieldsets: &'a BTreeMap<String, Vec<PlannedField>>,
        budget: Option<&'a SeaOrmRuntimeBudget>,
        included: &'a mut BTreeMap<(String, String), AdapterResource>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let registry = self
                .registry
                .as_ref()
                .ok_or_else(|| "SeaORM query adapter has no resource registry".to_owned())?;
            let definition = registry
                .resource(resource_type)
                .map_err(|error| error.to_string())?;
            for relationship in definition.relationships() {
                let include = includes
                    .iter()
                    .find(|include| include.public_name == relationship.public_name());
                let linkage_requested = relationship
                    .allows(crate::registry::RelationshipPermission::Read)
                    && matches!(
                        relationship.cardinality(),
                        Some(crate::registry::RelationshipCardinality::ToMany)
                    );
                if include.is_none() && !linkage_requested {
                    continue;
                }
                let pairs = self
                    .relationship_resources(resource_type, relationship, sources, budget)
                    .await?;
                if matches!(
                    relationship.cardinality(),
                    Some(crate::registry::RelationshipCardinality::ToMany)
                ) {
                    let mut linkage =
                        BTreeMap::<String, Vec<crate::document::ResourceIdentifier>>::new();
                    for (source_id, related) in &pairs {
                        linkage.entry(source_id.clone()).or_default().push(
                            crate::document::ResourceIdentifier {
                                type_name: relationship.target_type().to_owned(),
                                id: Some(related.id.clone()),
                                ..crate::document::ResourceIdentifier::default()
                            },
                        );
                    }
                    for source in sources.iter_mut() {
                        source.relationships.insert(
                            relationship.model_field().to_owned(),
                            crate::document::Relationship {
                                data: Some(crate::document::RelationshipData::Many(
                                    linkage.remove(&source.id).unwrap_or_default(),
                                )),
                                ..crate::document::Relationship::default()
                            },
                        );
                    }
                }
                let Some(include) = include else {
                    continue;
                };
                let mut next = BTreeMap::<(String, String), AdapterResource>::new();
                for (_, resource) in pairs {
                    next.entry((relationship.target_type().to_owned(), resource.id.clone()))
                        .or_insert(resource);
                }
                let mut next = next.into_values().collect::<Vec<_>>();
                self.expand_standard_level(
                    relationship.target_type(),
                    &mut next,
                    &include.children,
                    fieldsets,
                    budget,
                    included,
                )
                .await?;
                for resource in next {
                    let key = (relationship.target_type().to_owned(), resource.id.clone());
                    if let Some(existing) = included.get_mut(&key) {
                        existing.relationships.extend(resource.relationships);
                    } else {
                        included.insert(key, resource);
                    }
                }
            }
            let _ = fieldsets;
            Ok(())
        })
    }
}

#[async_trait]
impl QueryResourceAdapter for SeaOrmQueryAdapter {
    fn validate_registry(&self, registry: &ResourceRegistry) -> Result<(), String> {
        if self.registry.as_ref() != Some(registry) {
            return Err("query adapter executors do not share the supplied registry".to_owned());
        }
        for resource_type in self.executors.keys() {
            registry.resource(resource_type).map_err(|_| {
                format!("executor for unregistered resource type `{resource_type}`")
            })?;
        }
        for resource in registry.resources() {
            if !self.executors.contains_key(resource.type_name()) {
                return Err(format!(
                    "no SeaORM query executor is registered for `{}`",
                    resource.type_name()
                ));
            }
            for relationship in resource.relationships() {
                let includes =
                    relationship.allows(crate::registry::RelationshipPermission::Include);
                let linkage = relationship.allows(crate::registry::RelationshipPermission::Read)
                    && relationship.cardinality()
                        == Some(crate::registry::RelationshipCardinality::ToMany);
                if !includes && !linkage {
                    continue;
                }
                match relationship.storage() {
                    RelationshipStorage::ToOneForeignKey { .. } => {
                        if !self.executors.contains_key(relationship.target_type()) {
                            return Err(format!(
                                "no SeaORM query executor is registered for relationship target `{}`",
                                relationship.target_type()
                            ));
                        }
                    }
                    RelationshipStorage::ToManyForeignKey {
                        foreign_key_field, ..
                    } => {
                        self.executors
                            .get(relationship.target_type())
                            .ok_or_else(|| {
                                format!(
                                    "no SeaORM query executor is registered for relationship target `{}`",
                                    relationship.target_type()
                                )
                            })?
                            .validate_column(foreign_key_field)
                            .map_err(|error| {
                                format!(
                                    "invalid foreign key for `{}.{}`: {error}",
                                    resource.type_name(),
                                    relationship.public_name()
                                )
                            })?;
                    }
                    RelationshipStorage::JoinTable { .. } => {
                        if !self.join_table_queries.contains_key(&(
                            resource.type_name().to_owned(),
                            relationship.public_name().to_owned(),
                        )) {
                            return Err(format!(
                                "no typed join-table entity is registered for `{}.{}`",
                                resource.type_name(),
                                relationship.public_name()
                            ));
                        }
                    }
                    RelationshipStorage::Custom => {
                        if includes
                            && !self.executors[resource.type_name()].has_custom_include_loader()
                        {
                            return Err(format!(
                                "relationship `{}.{}` requires a custom include loader",
                                resource.type_name(),
                                relationship.public_name()
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn collection(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.executors
            .get(resource.type_name())
            .ok_or(QueryAdapterError::ReadFailed)?
            .collection(plan, None, Some(self))
            .await
    }

    async fn resource(
        &self,
        resource: &ResourceDefinition,
        id: &str,
        plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.executors
            .get(resource.type_name())
            .ok_or(QueryAdapterError::ReadFailed)?
            .resource(id, plan, None, Some(self))
            .await
    }

    async fn collection_with_limits(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
        limits: &ExecutionLimits,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.executors
            .get(resource.type_name())
            .ok_or(QueryAdapterError::ReadFailed)?
            .collection(plan, Some(limits), Some(self))
            .await
    }

    async fn resource_with_limits(
        &self,
        resource: &ResourceDefinition,
        id: &str,
        plan: &ReadPlan,
        limits: &ExecutionLimits,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.executors
            .get(resource.type_name())
            .ok_or(QueryAdapterError::ReadFailed)?
            .resource(id, plan, Some(limits), Some(self))
            .await
    }
}

#[async_trait]
impl StandardIncludeLoader for SeaOrmQueryAdapter {
    async fn load_standard_included(
        &self,
        resource_type: &str,
        roots: &mut [AdapterResource],
        includes: &[IncludeNode],
        fieldsets: &BTreeMap<String, Vec<PlannedField>>,
        budget: Option<&SeaOrmRuntimeBudget>,
    ) -> Result<Vec<IncludedResource>, String> {
        let mut included = BTreeMap::new();
        self.expand_standard_level(
            resource_type,
            roots,
            includes,
            fieldsets,
            budget,
            &mut included,
        )
        .await?;
        Ok(included
            .into_iter()
            .map(|((resource_type, _), resource)| IncludedResource {
                resource_type,
                resource,
            })
            .collect())
    }
}

/// An invalid SeaORM adapter registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeaOrmAdapterConfigurationError {
    /// Multiple handlers were registered for the same resource type.
    DuplicateQueryExecutor(String),
    /// Query executors were constructed from different resource registries.
    RegistryMismatch,
    /// A typed relationship executor does not match its registry mapping.
    InvalidRelationshipMapping(String),
}

impl fmt::Display for SeaOrmAdapterConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateQueryExecutor(resource_type) => write!(
                formatter,
                "a SeaORM query executor is already registered for `{resource_type}`"
            ),
            Self::RegistryMismatch => {
                formatter.write_str("SeaORM query executors use different resource registries")
            }
            Self::InvalidRelationshipMapping(message) => {
                write!(formatter, "invalid SeaORM relationship mapping: {message}")
            }
        }
    }
}

impl std::error::Error for SeaOrmAdapterConfigurationError {}

fn query_adapter_error(error: SeaOrmExecutionError) -> QueryAdapterError {
    match error {
        SeaOrmExecutionError::NotAuthorized => QueryAdapterError::NotAuthorized,
        SeaOrmExecutionError::LimitExceeded(_) => QueryAdapterError::LimitExceeded,
        SeaOrmExecutionError::CollectionQueryInResourcePlan => {
            QueryAdapterError::ResourceReadUnsupported
        }
        _ => QueryAdapterError::ReadFailed,
    }
}

fn validate_page_plan(page: Page) -> Result<(), SeaOrmExecutionError> {
    if page.number == 0 {
        return Err(SeaOrmExecutionError::InvalidPagePlan(
            "page number must be positive",
        ));
    }
    if page.size == 0 {
        return Err(SeaOrmExecutionError::InvalidPagePlan(
            "page size must be positive",
        ));
    }
    if page.limit == 0 {
        return Err(SeaOrmExecutionError::InvalidPagePlan(
            "page limit must be positive",
        ));
    }
    if page.limit != page.size {
        return Err(SeaOrmExecutionError::InvalidPagePlan(
            "page limit must match page size",
        ));
    }
    let expected_offset =
        (page.number - 1)
            .checked_mul(page.size)
            .ok_or(SeaOrmExecutionError::InvalidPagePlan(
                "page offset overflows",
            ))?;
    if page.offset != expected_offset {
        return Err(SeaOrmExecutionError::InvalidPagePlan(
            "page offset does not match page number and size",
        ));
    }
    Ok(())
}

fn column<E>(model_field: &str) -> Result<E::Column, SeaOrmExecutionError>
where
    E: EntityTrait,
    E::Column: FromStr,
{
    E::Column::from_str(model_field)
        .map_err(|_| SeaOrmExecutionError::UnknownModelField(model_field.to_owned()))
}

fn validate_filter_mappings(
    definition: &ResourceDefinition,
    plan: &ReadPlan,
) -> Result<(), SeaOrmExecutionError> {
    fn validate_expression(
        definition: &ResourceDefinition,
        resource_type: &str,
        expression: &FilterExpression,
    ) -> Result<(), SeaOrmExecutionError> {
        match expression {
            FilterExpression::Equals { model_field, .. } => {
                if !definition.attributes().iter().any(|attribute| {
                    attribute.model_field() == model_field && attribute.is_filterable()
                }) {
                    return Err(SeaOrmExecutionError::InvalidFilterField {
                        resource_type: resource_type.to_owned(),
                        model_field: model_field.clone(),
                    });
                }
            }
            FilterExpression::And(children) | FilterExpression::Or(children) => {
                for child in children {
                    validate_expression(definition, resource_type, child)?;
                }
            }
            FilterExpression::Not(child) => {
                validate_expression(definition, resource_type, child)?;
            }
        }
        Ok(())
    }

    if let Some(filter) = &plan.filter {
        validate_expression(definition, &plan.resource_type, filter)?;
    }
    Ok(())
}

fn validate_sort_mappings(
    definition: &ResourceDefinition,
    plan: &ReadPlan,
) -> Result<(), SeaOrmExecutionError> {
    for sort in &plan.sort {
        let is_registered_sort =
            definition
                .attribute_by_name(&sort.public_name)
                .is_some_and(|attribute| {
                    attribute.model_field() == sort.model_field && attribute.is_sortable()
                });
        if !is_registered_sort {
            return Err(SeaOrmExecutionError::InvalidSortField {
                resource_type: plan.resource_type.clone(),
                public_name: sort.public_name.clone(),
            });
        }
    }
    Ok(())
}

fn validate_fieldset_mappings(
    registry: &ResourceRegistry,
    plan: &ReadPlan,
) -> Result<(), SeaOrmExecutionError> {
    for (resource_type, fields) in &plan.fieldsets {
        let definition = registry
            .resource(resource_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(resource_type.clone()))?;
        for field in fields {
            let (public_name, mapping_is_valid) = match field {
                PlannedField::Attribute {
                    public_name,
                    model_field,
                } => (
                    public_name,
                    definition
                        .attribute_by_name(public_name)
                        .is_some_and(|mapping| mapping.model_field() == model_field),
                ),
                PlannedField::Relationship {
                    public_name,
                    model_field,
                    target_type,
                } => (
                    public_name,
                    definition
                        .relationship_by_name(public_name)
                        .is_some_and(|mapping| {
                            mapping.model_field() == model_field
                                && mapping.target_type() == target_type
                        }),
                ),
            };
            if !mapping_is_valid {
                return Err(SeaOrmExecutionError::InvalidFieldsetField {
                    resource_type: resource_type.clone(),
                    public_name: public_name.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_include_mappings(
    registry: &ResourceRegistry,
    resource_type: &str,
    includes: &[IncludeNode],
) -> Result<(), SeaOrmExecutionError> {
    let mut pending = vec![(resource_type.to_owned(), includes)];
    while let Some((current_type, nodes)) = pending.pop() {
        let definition = registry
            .resource(&current_type)
            .map_err(|_| SeaOrmExecutionError::UnknownResourceType(current_type.clone()))?;
        for include in nodes {
            let relationship = definition.relationship_by_name(&include.public_name);
            if !relationship.is_some_and(|mapping| {
                mapping.model_field() == include.model_field
                    && mapping.target_type() == include.target_type
            }) {
                return Err(SeaOrmExecutionError::InvalidIncludeRelationship {
                    resource_type: current_type.clone(),
                    public_name: include.public_name.clone(),
                });
            }
            pending.push((include.target_type.clone(), &include.children));
        }
    }
    Ok(())
}

fn filter_condition<E, C>(
    expression: &FilterExpression,
    codec: &C,
) -> Result<Condition, SeaOrmExecutionError>
where
    E: EntityTrait,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmFilterValueCodec,
{
    match expression {
        FilterExpression::Equals { model_field, value } => {
            let column = column::<E>(model_field)?;
            Ok(match value {
                FilterValue::String(value) => {
                    let value =
                        codec
                            .encode_filter_value(model_field, value)
                            .map_err(|message| SeaOrmExecutionError::InvalidFilterValue {
                                model_field: model_field.clone(),
                                value: value.clone(),
                                message,
                            })?;
                    column.eq(value).into_condition()
                }
                FilterValue::Null => column.is_null().into_condition(),
            })
        }
        FilterExpression::And(children) => {
            let mut condition = Condition::all();
            for child in children {
                condition = condition.add(filter_condition::<E, C>(child, codec)?);
            }
            Ok(condition)
        }
        FilterExpression::Or(children) => {
            let mut condition = Condition::any();
            for child in children {
                condition = condition.add(filter_condition::<E, C>(child, codec)?);
            }
            Ok(condition)
        }
        FilterExpression::Not(child) => Ok(Condition::all()
            .add(filter_condition::<E, C>(child, codec)?)
            .not()),
    }
}
