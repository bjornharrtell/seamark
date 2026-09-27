//! Database-side execution of validated read plans with SeaORM.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, Order, QueryFilter, QueryOrder,
    QuerySelect, Value, sea_query::IntoCondition,
};
use serde_json::Value as JsonValue;

use crate::http::AdapterResource;
use crate::query::{
    FilterExpression, FilterValue, IncludeNode, Page, PlannedField, ReadPlan, SortDirection,
};
use crate::registry::{ResourceDefinition, ResourceRegistry};

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
}

/// A codec suitable for both SeaORM query filtering and mutation handling.
pub trait SeaOrmValueCodec: SeaOrmFilterValueCodec + SeaOrmMutationValueCodec {}

impl<T> SeaOrmValueCodec for T where T: SeaOrmFilterValueCodec + SeaOrmMutationValueCodec {}

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
#[async_trait]
pub trait SeaOrmIncludeLoader<E>: Send + Sync
where
    E: EntityTrait,
{
    /// Loads resources requested by the validated include tree.
    ///
    /// # Errors
    ///
    /// Returns an application-level error if relation loading fails.
    async fn load_included(
        &self,
        database: &DatabaseConnection,
        roots: &[E::Model],
        includes: &[IncludeNode],
        fieldsets: &std::collections::BTreeMap<String, Vec<PlannedField>>,
    ) -> Result<Vec<IncludedResource>, String>;
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
    fn validate_limits(&self, plan: &ReadPlan) -> Result<(), String>;
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
    M: Fn(&E::Model) -> AdapterResource + Send + Sync,
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
    M: Fn(&E::Model) -> AdapterResource + Send + Sync,
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
        validate_filter_mappings(definition, plan)?;
        validate_sort_mappings(definition, plan)?;
        guard
            .validate_limits(plan)
            .map_err(SeaOrmExecutionError::LimitExceeded)?;
        if !guard.authorize(plan).await {
            return Err(SeaOrmExecutionError::NotAuthorized);
        }
        if !plan.includes.is_empty() && include_loader.is_none() {
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
        let resources = rows
            .iter()
            .map(|row| project_record(definition, (self.mapper)(row), fieldset))
            .collect();

        let included = if let Some(loader) = include_loader.filter(|_| !plan.includes.is_empty()) {
            loader
                .load_included(database, &rows, &plan.includes, &plan.fieldsets)
                .await
                .map_err(SeaOrmExecutionError::IncludeLoader)?
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
                    included.resource = project_record(definition, included.resource, fieldset);
                    Ok(included)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

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
        guard
            .validate_limits(plan)
            .map_err(SeaOrmExecutionError::LimitExceeded)?;
        if !guard.authorize(plan).await {
            return Err(SeaOrmExecutionError::NotAuthorized);
        }
        if !plan.includes.is_empty() && include_loader.is_none() {
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
        let resource = project_record(definition, (self.mapper)(&row), fieldset);
        let included = if let Some(loader) = include_loader.filter(|_| !plan.includes.is_empty()) {
            loader
                .load_included(
                    database,
                    std::slice::from_ref(&row),
                    &plan.includes,
                    &plan.fieldsets,
                )
                .await
                .map_err(SeaOrmExecutionError::IncludeLoader)?
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
                    included.resource = project_record(definition, included.resource, fieldset);
                    Ok(included)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        Ok(Some(SeaOrmResourceReadResult { resource, included }))
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

fn project_record(
    definition: &ResourceDefinition,
    mut resource: AdapterResource,
    fieldset: Option<&[PlannedField]>,
) -> AdapterResource {
    resource.attributes.retain(|model_field, _| {
        definition
            .attributes()
            .iter()
            .any(|attribute| attribute.model_field() == model_field)
    });
    resource.relationships.retain(|model_field, _| {
        definition
            .relationships()
            .iter()
            .any(|relationship| relationship.model_field() == model_field)
    });
    if let Some(fields) = fieldset {
        project_record_fields(&mut resource, fields);
    }
    resource
}

fn project_record_fields(resource: &mut AdapterResource, fields: &[PlannedField]) {
    let attributes = fields
        .iter()
        .filter_map(|field| match field {
            PlannedField::Attribute { model_field, .. } => Some(model_field.as_str()),
            PlannedField::Relationship { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let relationships = fields
        .iter()
        .filter_map(|field| match field {
            PlannedField::Relationship { model_field, .. } => Some(model_field.as_str()),
            PlannedField::Attribute { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    resource
        .attributes
        .retain(|model_field, _| attributes.contains(model_field.as_str()));
    resource
        .relationships
        .retain(|model_field, _| relationships.contains(model_field.as_str()));
}
