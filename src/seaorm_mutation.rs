//! Typed SeaORM mutation handlers for Atomic Operations.

use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, IntoActiveModel, ModelTrait,
    QueryFilter, Value,
};
use serde_json::{Value as JsonValue, json};

use crate::atomic::{
    AtomicOperationHandler, AtomicOperationOutcome, AtomicResourceChangeset,
    AtomicResourceReference, AtomicResult, AtomicTarget, LocalIdMap, PlannedOperation,
};
use crate::document::{RelationshipData, ResourceIdentifier};
use crate::registry::{RegistryError, ResourceDefinition, ResourceRegistry};
use crate::seaorm::SeaOrmMutationValueCodec;

/// Executes operations for one typed entity using explicit field/value mapping.
#[async_trait]
pub trait SeaOrmAtomicOperationExecutor: Send + Sync {
    /// Returns whether this handler can execute the planned operation.
    fn supports(&self, operation: &PlannedOperation) -> bool;

    /// Executes the operation using the shared transaction.
    ///
    /// # Errors
    ///
    /// Returns a mapping, database, or unsupported-operation error.
    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String>;
}

/// Dispatches planned operations to the first supporting typed executor.
pub struct SeaOrmAtomicOperationDispatcher {
    executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>>,
}

impl SeaOrmAtomicOperationDispatcher {
    /// Creates a dispatcher with deterministic first-match executor ordering.
    #[must_use]
    pub fn new(executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>>) -> Self {
        Self { executors }
    }
}

#[async_trait]
impl AtomicOperationHandler for SeaOrmAtomicOperationDispatcher {
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        for executor in &self.executors {
            if executor.supports(operation) {
                return executor.execute(transaction, operation, local_ids).await;
            }
        }
        Err("no SeaORM mutation executor supports this operation".to_owned())
    }
}

/// Executes to-many relationship membership operations against an explicit
/// SeaORM join-table entity.
///
/// The source resource, public relationship, and join-table columns are
/// configured explicitly because registry relationship fields do not encode
/// association cardinality or join-table structure. Other association shapes
/// remain available to custom [`SeaOrmAtomicOperationExecutor`] implementations.
pub struct SeaOrmJoinTableMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    source_type: String,
    model_field: String,
    target_type: String,
    source_column: String,
    target_column: String,
    value_codec: C,
    entity: PhantomData<fn() -> E>,
}

impl<E, C> SeaOrmJoinTableMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    /// Creates a typed executor for a declared relationship backed by a
    /// two-column join table.
    ///
    /// `source_column` and `target_column` are SeaORM column names on `E`;
    /// the supplied codec encodes the corresponding resource identifiers.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource or relationship is not registered or
    /// either join-table column is not present on the typed entity.
    pub fn new(
        registry: &ResourceRegistry,
        source_type: &str,
        relationship_name: &str,
        source_column: impl Into<String>,
        target_column: impl Into<String>,
        value_codec: C,
    ) -> Result<Self, String> {
        let source = registry
            .resource(source_type)
            .map_err(|error| error.to_string())?;
        let relationship = source
            .relationship_by_name(relationship_name)
            .ok_or_else(|| {
                format!("relationship `{relationship_name}` is not registered for `{source_type}`")
            })?;
        let target = registry
            .resource(relationship.target_type())
            .map_err(|error| error.to_string())?;
        let source_column = source_column.into();
        let target_column = target_column.into();
        if source_column == target_column {
            return Err("join-table source and target columns must be different".to_owned());
        }
        E::Column::from_str(&source_column)
            .map_err(|_| format!("join-table field `{source_column}` is not a SeaORM column"))?;
        E::Column::from_str(&target_column)
            .map_err(|_| format!("join-table field `{target_column}` is not a SeaORM column"))?;

        Ok(Self {
            source_type: source_type.to_owned(),
            model_field: relationship.model_field().to_owned(),
            target_type: target.type_name().to_owned(),
            source_column,
            target_column,
            value_codec,
            entity: PhantomData,
        })
    }

    fn supports_membership_operation(&self, operation: &PlannedOperation) -> bool {
        matches!(
            operation,
            PlannedOperation::AddRelationshipMembers {
                reference,
                model_field,
                ..
            } | PlannedOperation::RemoveRelationshipMembers {
                reference,
                model_field,
                ..
            } if reference.type_name == self.source_type && model_field == &self.model_field
        )
    }

    fn encode_identifier(&self, model_field: &str, identifier: &str) -> Result<Value, String> {
        self.value_codec
            .encode_mutation_value(model_field, &JsonValue::String(identifier.to_owned()))
    }
}

#[async_trait]
impl<E, C> SeaOrmAtomicOperationExecutor for SeaOrmJoinTableMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    fn supports(&self, operation: &PlannedOperation) -> bool {
        self.supports_membership_operation(operation)
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let (reference, identifiers, add) = match operation {
            PlannedOperation::AddRelationshipMembers {
                reference, data, ..
            } => (reference, data, true),
            PlannedOperation::RemoveRelationshipMembers {
                reference, data, ..
            } => (reference, data, false),
            _ => return Err("unsupported join-table relationship operation".to_owned()),
        };
        if !self.supports_membership_operation(operation) {
            return Err("join-table relationship mapping does not match operation".to_owned());
        }

        let source = local_ids.resolve_reference(reference)?;
        if source.type_name != self.source_type {
            return Err("relationship owner type does not match join-table mapping".to_owned());
        }
        let source_id = source
            .id
            .ok_or_else(|| "relationship owner has no persistent identifier".to_owned())?;
        let source_value = self.encode_identifier(&self.source_column, &source_id)?;
        let source_column = E::Column::from_str(&self.source_column).map_err(|_| {
            format!(
                "join-table field `{}` is not a SeaORM column",
                self.source_column
            )
        })?;
        let target_column = E::Column::from_str(&self.target_column).map_err(|_| {
            format!(
                "join-table field `{}` is not a SeaORM column",
                self.target_column
            )
        })?;

        let target_values = identifiers
            .iter()
            .map(|identifier| {
                let target = local_ids.resolve(identifier)?;
                if target.type_name != self.target_type {
                    return Err(format!(
                        "relationship member type `{}` does not match `{}`",
                        target.type_name, self.target_type
                    ));
                }
                let id = target
                    .id
                    .ok_or_else(|| "relationship member has no persistent identifier".to_owned())?;
                self.encode_identifier(&self.target_column, &id)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if add {
            for target_value in target_values {
                let mut active_model = <E::ActiveModel as Default>::default();
                active_model
                    .try_set(source_column, source_value.clone())
                    .map_err(|error| format!("could not map join-table source: {error}"))?;
                active_model
                    .try_set(target_column, target_value)
                    .map_err(|error| format!("could not map join-table target: {error}"))?;
                active_model
                    .insert(transaction)
                    .await
                    .map_err(|error| format!("join-table insert failed: {error}"))?;
            }
        } else if !target_values.is_empty() {
            E::delete_many()
                .filter(source_column.eq(source_value))
                .filter(target_column.is_in(target_values))
                .exec(transaction)
                .await
                .map_err(|error| format!("join-table delete failed: {error}"))?;
        }

        Ok(AtomicOperationOutcome::default())
    }
}

/// A SeaORM CRUD executor bound to one public resource and entity type.
///
/// Attribute and identifier conversion is explicit. Mapped to-one
/// relationships are stored in the configured foreign-key field. To-many
/// relationship operations and `href` targets remain application-defined and
/// can be handled by another [`SeaOrmAtomicOperationExecutor`] in the
/// dispatcher.
pub struct SeaOrmResourceMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: FromStr,
    C: SeaOrmMutationValueCodec,
{
    definition: ResourceDefinition,
    value_codec: C,
    entity: PhantomData<fn() -> E>,
}

impl<E, C> SeaOrmResourceMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: FromStr,
    C: SeaOrmMutationValueCodec,
{
    /// Binds this typed executor to a registered public resource.
    /// # Errors
    ///
    /// Returns an error if the resource type is not registered.
    pub fn new(
        registry: &ResourceRegistry,
        resource_type: &str,
        value_codec: C,
    ) -> Result<Self, RegistryError> {
        let definition = registry.resource(resource_type)?.clone();
        Ok(Self {
            definition,
            value_codec,
            entity: PhantomData,
        })
    }

    fn supports_resource_changeset(&self, changeset: &AtomicResourceChangeset) -> bool {
        changeset.type_name == self.definition.type_name()
            && changeset
                .relationships
                .as_ref()
                .is_none_or(|relationships| {
                    relationships.values().all(|relationship| {
                        matches!(
                            relationship.data.as_ref(),
                            None | Some(RelationshipData::Null | RelationshipData::One(_))
                        )
                    })
                })
    }

    fn column(&self, model_field: &str) -> Result<E::Column, String> {
        E::Column::from_str(model_field)
            .map_err(|_| format!("model field `{model_field}` is not a SeaORM column"))
    }

    fn encode(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        self.value_codec.encode_mutation_value(model_field, value)
    }

    fn set_field(
        &self,
        active_model: &mut E::ActiveModel,
        model_field: &str,
        value: &JsonValue,
    ) -> Result<(), String> {
        let column = self.column(model_field)?;
        let value = self.encode(model_field, value)?;
        active_model
            .try_set(column, value)
            .map_err(|error| format!("could not map field `{model_field}`: {error}"))
    }

    fn apply_changeset(
        &self,
        active_model: &mut E::ActiveModel,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
        include_identifier: bool,
    ) -> Result<(), String> {
        if include_identifier {
            if let Some(id) = &changeset.id {
                self.set_field(
                    active_model,
                    &changeset.identifier_field,
                    &JsonValue::String(id.clone()),
                )?;
            }
        }
        if let Some(attributes) = &changeset.attributes {
            for (model_field, value) in attributes {
                self.set_field(active_model, model_field, value)?;
            }
        }
        if let Some(relationships) = &changeset.relationships {
            for (model_field, relationship) in relationships {
                let Some(data) = &relationship.data else {
                    continue;
                };
                let value = match data {
                    RelationshipData::Null => JsonValue::Null,
                    RelationshipData::One(identifier) => {
                        let identifier = local_ids.resolve(identifier)?;
                        JsonValue::String(identifier.id.ok_or_else(|| {
                            "resolved relationship identity has no persistent `id`".to_owned()
                        })?)
                    }
                    RelationshipData::Many(_) => {
                        return Err(format!(
                            "to-many relationship field `{model_field}` requires an application executor"
                        ));
                    }
                };
                self.set_field(active_model, model_field, &value)?;
            }
        }
        Ok(())
    }

    fn target_identity(
        &self,
        target: &AtomicTarget,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicResourceReference, String> {
        match target {
            AtomicTarget::Reference(reference)
                if reference.type_name == self.definition.type_name() =>
            {
                local_ids.resolve_reference(reference)
            }
            AtomicTarget::Reference(reference) => Err(format!(
                "target type `{}` does not match resource `{}`",
                reference.type_name,
                self.definition.type_name()
            )),
            AtomicTarget::Href(_) => {
                Err("`href` target resolution requires an application executor".to_owned())
            }
        }
    }

    fn identifier_value(&self, id: &str) -> Result<Value, String> {
        self.encode(
            self.definition.identifier_field(),
            &JsonValue::String(id.to_owned()),
        )
    }

    fn decode_identifier(&self, value: &Value) -> Result<String, String> {
        self.value_codec
            .decode_identifier(self.definition.identifier_field(), value)
    }

    async fn add(
        &self,
        transaction: &DatabaseTransaction,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        self.apply_changeset(&mut active_model, changeset, local_ids, true)?;
        let model = active_model
            .insert(transaction)
            .await
            .map_err(|error| format!("resource create failed: {error}"))?;
        let identifier = self.column(changeset.identifier_field.as_str())?;
        let id_value = model.get(identifier);
        let id = self.decode_identifier(&id_value)?;
        let identity = ResourceIdentifier {
            type_name: changeset.type_name.clone(),
            id: Some(id.clone()),
            ..ResourceIdentifier::default()
        };
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": changeset.type_name, "id": id})),
                meta: None,
            },
            created_resource: changeset.lid.as_ref().map(|_| identity),
        })
    }

    async fn update(
        &self,
        transaction: &DatabaseTransaction,
        target: &AtomicTarget,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let target = self.target_identity(target, local_ids)?;
        let id = target
            .id
            .ok_or_else(|| "update target is missing its persistent `id`".to_owned())?;
        let id_column = self.column(&changeset.identifier_field)?;
        let id_value = self.identifier_value(&id)?;
        let has_updates = changeset
            .attributes
            .as_ref()
            .is_some_and(|attributes| !attributes.is_empty())
            || changeset
                .relationships
                .as_ref()
                .is_some_and(|relationships| {
                    relationships
                        .values()
                        .any(|relationship| relationship.data.is_some())
                });
        if !has_updates {
            let exists = E::find()
                .filter(id_column.eq(id_value))
                .one(transaction)
                .await
                .map_err(|error| format!("resource lookup failed: {error}"))?
                .is_some();
            if !exists {
                return Err("resource to update was not found".to_owned());
            }
            return Ok(AtomicOperationOutcome::default());
        }

        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        active_model
            .try_set(id_column, id_value)
            .map_err(|error| format!("could not map identifier field: {error}"))?;
        self.apply_changeset(&mut active_model, changeset, local_ids, false)?;
        active_model
            .update(transaction)
            .await
            .map_err(|error| format!("resource update failed: {error}"))?;
        Ok(AtomicOperationOutcome::default())
    }

    async fn remove(
        &self,
        transaction: &DatabaseTransaction,
        target: &AtomicTarget,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let target = self.target_identity(target, local_ids)?;
        let id = target
            .id
            .ok_or_else(|| "remove target is missing its persistent `id`".to_owned())?;
        let id_column = self.column(self.definition.identifier_field())?;
        let result = E::delete_many()
            .filter(id_column.eq(self.identifier_value(&id)?))
            .exec(transaction)
            .await
            .map_err(|error| format!("resource delete failed: {error}"))?;
        if result.rows_affected == 0 {
            return Err("resource to remove was not found".to_owned());
        }
        Ok(AtomicOperationOutcome::default())
    }

    async fn update_relationship(
        &self,
        transaction: &DatabaseTransaction,
        reference: &crate::atomic::AtomicResourceReference,
        model_field: &str,
        data: &RelationshipData,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if reference.type_name != self.definition.type_name() {
            return Err("relationship owner type does not match this entity".to_owned());
        }
        let target = AtomicTarget::Reference(reference.clone());
        let identity = self.target_identity(&target, local_ids)?;
        let id = identity
            .id
            .ok_or_else(|| "relationship owner is missing its persistent `id`".to_owned())?;
        let value = match data {
            RelationshipData::Null => JsonValue::Null,
            RelationshipData::One(identifier) => {
                let identifier = local_ids.resolve(identifier)?;
                JsonValue::String(identifier.id.ok_or_else(|| {
                    "resolved relationship identity has no persistent `id`".to_owned()
                })?)
            }
            RelationshipData::Many(_) => {
                return Err(format!(
                    "to-many relationship field `{model_field}` requires an application executor"
                ));
            }
        };

        let id_column = self.column(self.definition.identifier_field())?;
        let id_value = self.identifier_value(&id)?;
        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        active_model
            .try_set(id_column, id_value)
            .map_err(|error| format!("could not map identifier field: {error}"))?;
        self.set_field(&mut active_model, model_field, &value)?;
        active_model
            .update(transaction)
            .await
            .map_err(|error| format!("relationship update failed: {error}"))?;
        Ok(AtomicOperationOutcome::default())
    }
}

#[async_trait]
impl<E, C> SeaOrmAtomicOperationExecutor for SeaOrmResourceMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Column: FromStr,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    C: SeaOrmMutationValueCodec,
{
    fn supports(&self, operation: &PlannedOperation) -> bool {
        match operation {
            PlannedOperation::AddResource {
                href, changeset, ..
            } => href.is_none() && self.supports_resource_changeset(changeset),
            PlannedOperation::UpdateResource {
                target, changeset, ..
            } => {
                matches!(target, AtomicTarget::Reference(reference) if reference.type_name == self.definition.type_name())
                    && self.supports_resource_changeset(changeset)
            }
            PlannedOperation::RemoveResource { target } => {
                matches!(target, AtomicTarget::Reference(reference) if reference.type_name == self.definition.type_name())
            }
            PlannedOperation::UpdateRelationship {
                reference, data, ..
            } => {
                reference.type_name == self.definition.type_name()
                    && matches!(data, RelationshipData::Null | RelationshipData::One(_))
            }
            PlannedOperation::AddRelationshipMembers { .. }
            | PlannedOperation::RemoveRelationshipMembers { .. } => false,
        }
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        match operation {
            PlannedOperation::AddResource { changeset, .. } => {
                self.add(transaction, changeset, local_ids).await
            }
            PlannedOperation::UpdateResource {
                target, changeset, ..
            } => self.update(transaction, target, changeset, local_ids).await,
            PlannedOperation::RemoveResource { target } => {
                self.remove(transaction, target, local_ids).await
            }
            PlannedOperation::UpdateRelationship {
                reference,
                model_field,
                data,
            } => {
                self.update_relationship(transaction, reference, model_field, data, local_ids)
                    .await
            }
            PlannedOperation::AddRelationshipMembers { .. }
            | PlannedOperation::RemoveRelationshipMembers { .. } => {
                Err("to-many relationship operations require an application executor".to_owned())
            }
        }
    }
}
