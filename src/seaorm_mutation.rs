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

/// A SeaORM CRUD executor bound to one public resource and entity type.
///
/// Attribute and identifier conversion is explicit. Mapped to-one
/// relationships are stored in the configured foreign-key field. To-many
/// relationship operations and `href` targets remain application-defined and
/// can be handled by another [`SeaOrmAtomicOperationExecutor`] in the
/// dispatcher.
pub struct SeaOrmResourceMutationHandler<E, F, I>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: FromStr,
    F: Fn(&str, &JsonValue) -> Result<Value, String> + Send + Sync,
    I: Fn(&str, &Value) -> Result<String, String> + Send + Sync,
{
    definition: ResourceDefinition,
    value_encoder: F,
    identifier_decoder: I,
    entity: PhantomData<fn() -> E>,
}

impl<E, F, I> SeaOrmResourceMutationHandler<E, F, I>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: FromStr,
    F: Fn(&str, &JsonValue) -> Result<Value, String> + Send + Sync,
    I: Fn(&str, &Value) -> Result<String, String> + Send + Sync,
{
    /// Binds this typed executor to a registered public resource.
    /// # Errors
    ///
    /// Returns an error if the resource type is not registered.
    pub fn new(
        registry: &ResourceRegistry,
        resource_type: &str,
        value_encoder: F,
        identifier_decoder: I,
    ) -> Result<Self, RegistryError> {
        let definition = registry.resource(resource_type)?.clone();
        Ok(Self {
            definition,
            value_encoder,
            identifier_decoder,
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
        (self.value_encoder)(model_field, value)
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
        (self.identifier_decoder)(self.definition.identifier_field(), value)
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
impl<E, F, I> SeaOrmAtomicOperationExecutor for SeaOrmResourceMutationHandler<E, F, I>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Column: FromStr,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    F: Fn(&str, &JsonValue) -> Result<Value, String> + Send + Sync,
    I: Fn(&str, &Value) -> Result<String, String> + Send + Sync,
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
