//! Typed SeaORM mutation handlers for Atomic Operations.

use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    IntoActiveModel, ModelTrait, QueryFilter, RuntimeErr, TransactionTrait, Value,
    sea_query::{Condition, Expr},
};
use serde_json::{Value as JsonValue, json};

use crate::atomic::{
    AtomicOperationFailure, AtomicOperationHandler, AtomicOperationOutcome,
    AtomicResourceChangeset, AtomicResourceData, AtomicResourceReference, AtomicResult,
    AtomicTarget, LocalIdMap, PlannedOperation,
};
use crate::document::{RelationshipData, ResourceIdentifier};
use crate::http::{
    MutationAdapterError, MutationCommand, MutationOutcome, MutationResourceAdapter,
};
use crate::registry::{RegistryError, ResourceDefinition, ResourceRegistry};
use crate::seaorm::SeaOrmMutationValueCodec;

fn is_unique_constraint_violation(error: &DbErr) -> bool {
    match error {
        DbErr::Exec(RuntimeErr::SqlxError(error)) | DbErr::Query(RuntimeErr::SqlxError(error)) => {
            error
                .as_database_error()
                .is_some_and(|database_error| database_error.is_unique_violation())
        }
        _ => false,
    }
}

fn is_foreign_key_violation(error: &DbErr) -> bool {
    match error {
        DbErr::Exec(RuntimeErr::SqlxError(error)) | DbErr::Query(RuntimeErr::SqlxError(error)) => {
            error
                .as_database_error()
                .is_some_and(|database_error| database_error.is_foreign_key_violation())
        }
        _ => false,
    }
}

/// Executes one base HTTP mutation with an explicit typed SeaORM mapping.
#[async_trait]
pub trait SeaOrmBaseMutationExecutor: Send + Sync {
    /// Returns whether this executor owns the resource and command.
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool;

    /// Executes the command using the adapter's shared transaction.
    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError>;
}

/// Runs each base HTTP mutation through one SeaORM transaction.
///
/// Executors are checked in order; the first matching typed mapping handles
/// the command. The adapter does not translate base HTTP commands into
/// Atomic Operations.
pub struct SeaOrmBaseMutationAdapter {
    database: DatabaseConnection,
    executors: Vec<Arc<dyn SeaOrmBaseMutationExecutor>>,
}

impl SeaOrmBaseMutationAdapter {
    /// Creates an adapter with deterministic first-match executor ordering.
    #[must_use]
    pub fn new(
        database: DatabaseConnection,
        executors: Vec<Arc<dyn SeaOrmBaseMutationExecutor>>,
    ) -> Self {
        Self {
            database,
            executors,
        }
    }
}

#[async_trait]
impl MutationResourceAdapter for SeaOrmBaseMutationAdapter {
    async fn execute(
        &self,
        resource: &ResourceDefinition,
        command: MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        let executor = self
            .executors
            .iter()
            .find(|executor| executor.supports(resource, &command))
            .ok_or(MutationAdapterError::Unsupported)?;
        let transaction = self
            .database
            .begin()
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
        match executor.execute(&transaction, resource, &command).await {
            Ok(outcome) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?;
                Ok(outcome)
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?;
                Err(error)
            }
        }
    }
}

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

    /// Executes the operation while preserving typed failure categories.
    async fn execute_with_failure(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        self.execute(transaction, operation, local_ids)
            .await
            .map_err(AtomicOperationFailure::Operation)
    }
}

/// Dispatches planned operations to typed executors.
///
/// If no executor handles a resource add or update directly, resource changes
/// containing to-many linkage are composed from the typed resource executor
/// and matching relationship executors within the same transaction.
pub struct SeaOrmAtomicOperationDispatcher {
    executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>>,
}

impl SeaOrmAtomicOperationDispatcher {
    /// Creates a dispatcher with deterministic first-match executor ordering.
    #[must_use]
    pub fn new(executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>>) -> Self {
        Self { executors }
    }

    async fn execute_to_many_replacements(
        &self,
        transaction: &DatabaseTransaction,
        reference: &AtomicResourceReference,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
    ) -> Result<(), AtomicOperationFailure> {
        for (model_field, relationship) in changeset.relationships.as_ref().into_iter().flatten() {
            let Some(RelationshipData::Many(identifiers)) = &relationship.data else {
                continue;
            };
            let operation = PlannedOperation::UpdateRelationship {
                reference: reference.clone(),
                model_field: model_field.clone(),
                data: RelationshipData::Many(identifiers.clone()),
            };
            let executor = self
                .executors
                .iter()
                .find(|executor| executor.supports(&operation))
                .ok_or_else(|| {
                    AtomicOperationFailure::Operation(format!(
                        "no SeaORM relationship executor supports field `{model_field}`"
                    ))
                })?;
            executor
                .execute_with_failure(transaction, &operation, local_ids)
                .await?;
        }
        Ok(())
    }

    async fn execute_composed_resource_add(
        &self,
        transaction: &DatabaseTransaction,
        href: &Option<String>,
        data: &AtomicResourceData,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let (resource_data, resource_changeset) = without_to_many_relationships(data, changeset);
        let operation = PlannedOperation::AddResource {
            href: href.clone(),
            data: resource_data,
            changeset: resource_changeset,
        };
        let executor = self
            .executors
            .iter()
            .find(|executor| executor.supports(&operation))
            .ok_or_else(|| {
                AtomicOperationFailure::Operation(
                    "no SeaORM mutation executor supports the resource add".to_owned(),
                )
            })?;
        let outcome = executor
            .execute_with_failure(transaction, &operation, local_ids)
            .await?;
        let reference = resource_add_reference(changeset, &outcome)
            .map_err(AtomicOperationFailure::Operation)?;
        self.execute_to_many_replacements(transaction, &reference, changeset, local_ids)
            .await?;
        Ok(outcome)
    }

    async fn execute_composed_resource_update(
        &self,
        transaction: &DatabaseTransaction,
        reference: &AtomicResourceReference,
        data: &AtomicResourceData,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let (resource_data, resource_changeset) = without_to_many_relationships(data, changeset);
        let operation = PlannedOperation::UpdateResource {
            target: AtomicTarget::Reference(reference.clone()),
            data: resource_data,
            changeset: resource_changeset,
        };
        let executor = self
            .executors
            .iter()
            .find(|executor| executor.supports(&operation))
            .ok_or_else(|| {
                AtomicOperationFailure::Operation(
                    "no SeaORM mutation executor supports the resource update".to_owned(),
                )
            })?;
        let outcome = executor
            .execute_with_failure(transaction, &operation, local_ids)
            .await?;
        self.execute_to_many_replacements(transaction, reference, changeset, local_ids)
            .await?;
        Ok(outcome)
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
        self.execute_operation_with_failure(transaction, operation, local_ids)
            .await
            .map_err(|failure| failure.to_string())
    }

    async fn execute_operation_with_failure(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        for executor in &self.executors {
            if executor.supports(operation) {
                return executor
                    .execute_with_failure(transaction, operation, local_ids)
                    .await;
            }
        }
        if let PlannedOperation::AddResource {
            href,
            data,
            changeset,
        } = operation
        {
            if has_to_many_relationships(changeset) {
                return self
                    .execute_composed_resource_add(transaction, href, data, changeset, local_ids)
                    .await;
            }
        }
        if let PlannedOperation::UpdateResource {
            target: AtomicTarget::Reference(reference),
            data,
            changeset,
        } = operation
        {
            if has_to_many_relationships(changeset) {
                return self
                    .execute_composed_resource_update(
                        transaction,
                        reference,
                        data,
                        changeset,
                        local_ids,
                    )
                    .await;
            }
        }
        Err(AtomicOperationFailure::Operation(
            "no SeaORM mutation executor supports this operation".to_owned(),
        ))
    }
}

fn has_to_many_relationships(changeset: &AtomicResourceChangeset) -> bool {
    changeset
        .relationships
        .as_ref()
        .is_some_and(|relationships| {
            relationships
                .values()
                .any(|relationship| matches!(&relationship.data, Some(RelationshipData::Many(_))))
        })
}

fn has_related_resource_reference(changeset: &AtomicResourceChangeset) -> bool {
    changeset
        .relationships
        .as_ref()
        .is_some_and(|relationships| {
            relationships.values().any(|relationship| {
                matches!(relationship.data.as_ref(), Some(RelationshipData::One(_)))
            })
        })
}

fn without_to_many_relationships(
    data: &AtomicResourceData,
    changeset: &AtomicResourceChangeset,
) -> (AtomicResourceData, AtomicResourceChangeset) {
    let mut data = data.clone();
    if let Some(relationships) = data.relationships.as_mut() {
        relationships.retain(|_, relationship| {
            !matches!(&relationship.data, Some(RelationshipData::Many(_)))
        });
    }

    let mut changeset = changeset.clone();
    if let Some(relationships) = changeset.relationships.as_mut() {
        relationships.retain(|_, relationship| {
            !matches!(&relationship.data, Some(RelationshipData::Many(_)))
        });
    }
    (data, changeset)
}

fn resource_add_reference(
    changeset: &AtomicResourceChangeset,
    outcome: &AtomicOperationOutcome,
) -> Result<AtomicResourceReference, String> {
    let identity = outcome
        .created_resource
        .clone()
        .or_else(|| {
            changeset.id.as_ref().map(|id| ResourceIdentifier {
                type_name: changeset.type_name.clone(),
                id: Some(id.clone()),
                ..ResourceIdentifier::default()
            })
        })
        .or_else(|| {
            let data = outcome.result.data.as_ref()?;
            let type_name = data.get("type")?.as_str()?;
            let id = data.get("id")?.as_str()?;
            Some(ResourceIdentifier {
                type_name: type_name.to_owned(),
                id: Some(id.to_owned()),
                ..ResourceIdentifier::default()
            })
        })
        .ok_or_else(|| {
            "resource add did not return an identity for its relationships".to_owned()
        })?;
    if identity.type_name != changeset.type_name {
        return Err("resource add identity type does not match its changeset".to_owned());
    }
    let id = identity
        .id
        .ok_or_else(|| "resource add identity has no persistent `id`".to_owned())?;
    Ok(AtomicResourceReference {
        type_name: changeset.type_name.clone(),
        id: Some(id),
        lid: None,
        relationship: None,
    })
}

/// Executes to-many relationship add, remove, and replacement operations
/// against an explicit SeaORM join-table entity.
///
/// The source resource, public relationship, and join-table columns are
/// configured explicitly because registry relationship fields do not encode
/// association cardinality or join-table structure. Other association shapes
/// remain available to custom [`SeaOrmAtomicOperationExecutor`] implementations.
/// This two-column mapping does not persist relationship member ordering.
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

#[derive(Clone, Copy)]
enum JoinTableOperation {
    Add,
    Remove,
    Replace,
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

    fn supports_relationship_operation(&self, operation: &PlannedOperation) -> bool {
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
        ) || matches!(
            operation,
            PlannedOperation::UpdateRelationship {
                reference,
                model_field,
                data: RelationshipData::Many(_),
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
        self.supports_relationship_operation(operation)
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.execute_with_failure(transaction, operation, local_ids)
            .await
            .map_err(|failure| failure.to_string())
    }

    async fn execute_with_failure(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let (reference, identifiers, action) = match operation {
            PlannedOperation::AddRelationshipMembers {
                reference, data, ..
            } => (reference, data.as_slice(), JoinTableOperation::Add),
            PlannedOperation::RemoveRelationshipMembers {
                reference, data, ..
            } => (reference, data.as_slice(), JoinTableOperation::Remove),
            PlannedOperation::UpdateRelationship {
                reference,
                data: RelationshipData::Many(identifiers),
                ..
            } => (
                reference,
                identifiers.as_slice(),
                JoinTableOperation::Replace,
            ),
            _ => {
                return Err(AtomicOperationFailure::Operation(
                    "unsupported join-table relationship operation".to_owned(),
                ));
            }
        };
        if !self.supports_relationship_operation(operation) {
            return Err(AtomicOperationFailure::Operation(
                "join-table relationship mapping does not match operation".to_owned(),
            ));
        }

        let source = local_ids.resolve_reference(reference)?;
        if source.type_name != self.source_type {
            return Err(AtomicOperationFailure::Operation(
                "relationship owner type does not match join-table mapping".to_owned(),
            ));
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

        if matches!(action, JoinTableOperation::Replace) {
            E::delete_many()
                .filter(source_column.eq(source_value.clone()))
                .exec(transaction)
                .await
                .map_err(|error| format!("join-table replacement delete failed: {error}"))?;
        } else if matches!(action, JoinTableOperation::Remove) && !target_values.is_empty() {
            E::delete_many()
                .filter(source_column.eq(source_value.clone()))
                .filter(target_column.is_in(target_values.clone()))
                .exec(transaction)
                .await
                .map_err(|error| format!("join-table delete failed: {error}"))?;
        }

        if matches!(
            action,
            JoinTableOperation::Add | JoinTableOperation::Replace
        ) {
            for target_value in target_values {
                if matches!(action, JoinTableOperation::Add)
                    && E::find()
                        .filter(source_column.eq(source_value.clone()))
                        .filter(target_column.eq(target_value.clone()))
                        .one(transaction)
                        .await
                        .map_err(|error| format!("join-table membership lookup failed: {error}"))?
                        .is_some()
                {
                    continue;
                }
                let mut active_model = <E::ActiveModel as Default>::default();
                active_model
                    .try_set(source_column, source_value.clone())
                    .map_err(|error| format!("could not map join-table source: {error}"))?;
                active_model
                    .try_set(target_column, target_value)
                    .map_err(|error| format!("could not map join-table target: {error}"))?;
                active_model.insert(transaction).await.map_err(|error| {
                    if is_foreign_key_violation(&error) {
                        AtomicOperationFailure::NotFound(
                            "a referenced relationship resource does not exist".to_owned(),
                        )
                    } else {
                        AtomicOperationFailure::Operation(format!(
                            "join-table insert failed: {error}"
                        ))
                    }
                })?;
            }
        }

        Ok(AtomicOperationOutcome::default())
    }
}

/// Executes to-many relationship add/remove/replacement through a nullable
/// foreign-key column on the related resource entity.
///
/// The relationship and both resource identifier mappings are explicit.
/// Adding or replacing members assigns unowned members to the source; it does
/// not reassign members owned by a different source. Removing members clears
/// the foreign key only when it currently points to the requested source.
/// Replacement clears the source's current members before assigning the new
/// set. The foreign-key column must be nullable. Association shapes that do
/// not fit this mapping remain available to custom executors.
pub struct SeaOrmToManyForeignKeyMutationHandler<E, C>
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
    target_identifier_field: String,
    foreign_key_field: String,
    value_codec: C,
    entity: PhantomData<fn() -> E>,
}

#[derive(Clone, Copy)]
enum ForeignKeyOperation {
    Add,
    Remove,
    Replace,
}

impl<E, C> SeaOrmToManyForeignKeyMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    /// Creates a typed executor for a to-many relationship represented by a
    /// nullable foreign key on the related entity.
    ///
    /// `foreign_key_field` is the target entity's SeaORM column that stores
    /// the source resource identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the source resource or relationship is not
    /// registered, or either target entity column is unknown.
    pub fn new(
        registry: &ResourceRegistry,
        source_type: &str,
        relationship_name: &str,
        foreign_key_field: impl Into<String>,
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
        let foreign_key_field = foreign_key_field.into();
        if foreign_key_field == target.identifier_field() {
            return Err(
                "to-many foreign-key and target identifier fields must be different".to_owned(),
            );
        }
        E::Column::from_str(target.identifier_field()).map_err(|_| {
            format!(
                "target identifier field `{}` is not a SeaORM column",
                target.identifier_field()
            )
        })?;
        E::Column::from_str(&foreign_key_field).map_err(|_| {
            format!("foreign-key field `{foreign_key_field}` is not a SeaORM column")
        })?;

        Ok(Self {
            source_type: source_type.to_owned(),
            model_field: relationship.model_field().to_owned(),
            target_type: target.type_name().to_owned(),
            target_identifier_field: target.identifier_field().to_owned(),
            foreign_key_field,
            value_codec,
            entity: PhantomData,
        })
    }

    fn supports_relationship_operation(&self, operation: &PlannedOperation) -> bool {
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
        ) || matches!(
            operation,
            PlannedOperation::UpdateRelationship {
                reference,
                model_field,
                data: RelationshipData::Many(_),
            } if reference.type_name == self.source_type && model_field == &self.model_field
        )
    }

    fn encode(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        self.value_codec.encode_mutation_value(model_field, value)
    }
}

#[async_trait]
impl<E, C> SeaOrmAtomicOperationExecutor for SeaOrmToManyForeignKeyMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    fn supports(&self, operation: &PlannedOperation) -> bool {
        self.supports_relationship_operation(operation)
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.execute_with_failure(transaction, operation, local_ids)
            .await
            .map_err(|failure| failure.to_string())
    }

    async fn execute_with_failure(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let (reference, identifiers, action) = match operation {
            PlannedOperation::AddRelationshipMembers {
                reference, data, ..
            } => (reference, data.as_slice(), ForeignKeyOperation::Add),
            PlannedOperation::RemoveRelationshipMembers {
                reference, data, ..
            } => (reference, data.as_slice(), ForeignKeyOperation::Remove),
            PlannedOperation::UpdateRelationship {
                reference,
                data: RelationshipData::Many(identifiers),
                ..
            } => (
                reference,
                identifiers.as_slice(),
                ForeignKeyOperation::Replace,
            ),
            _ => {
                return Err(AtomicOperationFailure::Operation(
                    "unsupported to-many foreign-key operation".to_owned(),
                ));
            }
        };
        if !self.supports_relationship_operation(operation) {
            return Err(AtomicOperationFailure::Operation(
                "to-many foreign-key mapping does not match operation".to_owned(),
            ));
        }

        let source = local_ids.resolve_reference(reference)?;
        if source.type_name != self.source_type {
            return Err(AtomicOperationFailure::Operation(
                "relationship owner type does not match foreign-key mapping".to_owned(),
            ));
        }
        let source_id = source
            .id
            .ok_or_else(|| "relationship owner has no persistent identifier".to_owned())?;
        let source_value = self.encode(&self.foreign_key_field, &JsonValue::String(source_id))?;
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
                self.encode(&self.target_identifier_field, &JsonValue::String(id))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let target_identifier_column =
            E::Column::from_str(&self.target_identifier_field).map_err(|_| {
                format!(
                    "target identifier field `{}` is not a SeaORM column",
                    self.target_identifier_field
                )
            })?;
        let foreign_key_column = E::Column::from_str(&self.foreign_key_field).map_err(|_| {
            format!(
                "foreign-key field `{}` is not a SeaORM column",
                self.foreign_key_field
            )
        })?;
        let null_value = match action {
            ForeignKeyOperation::Add => None,
            ForeignKeyOperation::Remove | ForeignKeyOperation::Replace => {
                Some(self.encode(&self.foreign_key_field, &JsonValue::Null)?)
            }
        };

        if matches!(action, ForeignKeyOperation::Replace) {
            E::update_many()
                .filter(foreign_key_column.eq(source_value.clone()))
                .col_expr(
                    foreign_key_column,
                    Expr::value(
                        null_value.clone().ok_or_else(|| {
                            "missing encoded nullable foreign-key value".to_owned()
                        })?,
                    ),
                )
                .exec(transaction)
                .await
                .map_err(|error| {
                    format!("to-many foreign-key replacement clear failed: {error}")
                })?;
        }

        for target_value in target_values {
            if matches!(action, ForeignKeyOperation::Add)
                && E::find()
                    .filter(target_identifier_column.eq(target_value.clone()))
                    .filter(foreign_key_column.eq(source_value.clone()))
                    .one(transaction)
                    .await
                    .map_err(|error| {
                        format!("to-many foreign-key membership lookup failed: {error}")
                    })?
                    .is_some()
            {
                continue;
            }
            let query = E::update_many()
                .filter(target_identifier_column.eq(target_value.clone()))
                .filter(match action {
                    ForeignKeyOperation::Add | ForeignKeyOperation::Replace => Condition::any()
                        .add(foreign_key_column.is_null())
                        .add(foreign_key_column.eq(source_value.clone())),
                    ForeignKeyOperation::Remove => {
                        Condition::all().add(foreign_key_column.eq(source_value.clone()))
                    }
                });
            let assigned_value = match action {
                ForeignKeyOperation::Add | ForeignKeyOperation::Replace => source_value.clone(),
                ForeignKeyOperation::Remove => null_value
                    .clone()
                    .ok_or_else(|| "missing encoded nullable foreign-key value".to_owned())?,
            };
            let result = query
                .col_expr(foreign_key_column, Expr::value(assigned_value))
                .exec(transaction)
                .await
                .map_err(|error| format!("to-many foreign-key update failed: {error}"))?;
            if matches!(
                action,
                ForeignKeyOperation::Add | ForeignKeyOperation::Replace
            ) && result.rows_affected == 0
            {
                let target_exists = E::find()
                    .filter(target_identifier_column.eq(target_value.clone()))
                    .one(transaction)
                    .await
                    .map_err(|error| format!("to-many foreign-key target lookup failed: {error}"))?
                    .is_some();
                if !target_exists {
                    return Err(AtomicOperationFailure::NotFound(
                        "a referenced relationship resource does not exist".to_owned(),
                    ));
                }
                return Err(AtomicOperationFailure::Operation(
                    "relationship member already belongs to another owner".to_owned(),
                ));
            }
        }

        Ok(AtomicOperationOutcome::default())
    }
}

/// A SeaORM CRUD executor bound to one public resource and entity type.
///
/// Attribute and identifier conversion is explicit. Mapped to-one
/// relationships are stored in the configured foreign-key field. To-many
/// relationships are declined by this resource executor; the dedicated
/// relationship executors support explicitly configured join-table and
/// nullable direct-foreign-key shapes. Other associations and `href` target
/// behavior remain available to custom [`SeaOrmAtomicOperationExecutor`]
/// implementations in the dispatcher.
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
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        self.apply_changeset(&mut active_model, changeset, local_ids, true)?;
        let model = active_model.insert(transaction).await.map_err(|error| {
            if is_unique_constraint_violation(&error) {
                AtomicOperationFailure::Conflict(
                    "the resource conflicts with existing data".to_owned(),
                )
            } else if has_related_resource_reference(changeset) && is_foreign_key_violation(&error)
            {
                AtomicOperationFailure::NotFound(
                    "a referenced relationship resource does not exist".to_owned(),
                )
            } else {
                AtomicOperationFailure::Operation(format!("resource create failed: {error}"))
            }
        })?;
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
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
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
                .map_err(|error| {
                    AtomicOperationFailure::Operation(format!("resource lookup failed: {error}"))
                })?
                .is_some();
            if !exists {
                return Err(AtomicOperationFailure::NotFound(
                    "resource to update was not found".to_owned(),
                ));
            }
            return Ok(AtomicOperationOutcome::default());
        }

        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        active_model
            .try_set(id_column, id_value)
            .map_err(|error| format!("could not map identifier field: {error}"))?;
        self.apply_changeset(&mut active_model, changeset, local_ids, false)?;
        active_model.update(transaction).await.map_err(|error| {
            if matches!(&error, DbErr::RecordNotUpdated) {
                AtomicOperationFailure::NotFound("resource to update was not found".to_owned())
            } else if has_related_resource_reference(changeset) && is_foreign_key_violation(&error)
            {
                AtomicOperationFailure::NotFound(
                    "a referenced relationship resource does not exist".to_owned(),
                )
            } else {
                AtomicOperationFailure::Operation(format!("resource update failed: {error}"))
            }
        })?;
        Ok(AtomicOperationOutcome::default())
    }

    async fn remove(
        &self,
        transaction: &DatabaseTransaction,
        target: &AtomicTarget,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        let target = self.target_identity(target, local_ids)?;
        let id = target
            .id
            .ok_or_else(|| "remove target is missing its persistent `id`".to_owned())?;
        let id_column = self.column(self.definition.identifier_field())?;
        let result = E::delete_many()
            .filter(id_column.eq(self.identifier_value(&id)?))
            .exec(transaction)
            .await
            .map_err(|error| {
                AtomicOperationFailure::Operation(format!("resource delete failed: {error}"))
            })?;
        if result.rows_affected == 0 {
            return Err(AtomicOperationFailure::NotFound(
                "resource to remove was not found".to_owned(),
            ));
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
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
        if reference.type_name != self.definition.type_name() {
            return Err(AtomicOperationFailure::Operation(
                "relationship owner type does not match this entity".to_owned(),
            ));
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
                return Err(AtomicOperationFailure::Operation(format!(
                    "to-many relationship field `{model_field}` requires an application executor"
                )));
            }
        };

        let id_column = self.column(self.definition.identifier_field())?;
        let id_value = self.identifier_value(&id)?;
        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        active_model
            .try_set(id_column, id_value)
            .map_err(|error| format!("could not map identifier field: {error}"))?;
        self.set_field(&mut active_model, model_field, &value)?;
        active_model.update(transaction).await.map_err(|error| {
            if matches!(&error, DbErr::RecordNotUpdated) {
                AtomicOperationFailure::NotFound("relationship owner was not found".to_owned())
            } else if matches!(data, RelationshipData::One(_)) && is_foreign_key_violation(&error) {
                AtomicOperationFailure::NotFound(
                    "a referenced relationship resource does not exist".to_owned(),
                )
            } else {
                AtomicOperationFailure::Operation(format!("relationship update failed: {error}"))
            }
        })?;
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
            PlannedOperation::AddResource { changeset, .. } => self
                .add(transaction, changeset, local_ids)
                .await
                .map_err(|failure| failure.to_string()),
            PlannedOperation::UpdateResource {
                target, changeset, ..
            } => self
                .update(transaction, target, changeset, local_ids)
                .await
                .map_err(|failure| failure.to_string()),
            PlannedOperation::RemoveResource { target } => self
                .remove(transaction, target, local_ids)
                .await
                .map_err(|failure| failure.to_string()),
            PlannedOperation::UpdateRelationship {
                reference,
                model_field,
                data,
            } => self
                .update_relationship(transaction, reference, model_field, data, local_ids)
                .await
                .map_err(|failure| failure.to_string()),
            PlannedOperation::AddRelationshipMembers { .. }
            | PlannedOperation::RemoveRelationshipMembers { .. } => {
                Err("to-many relationship operations require an application executor".to_owned())
            }
        }
    }

    async fn execute_with_failure(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, AtomicOperationFailure> {
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
            _ => self
                .execute(transaction, operation, local_ids)
                .await
                .map_err(AtomicOperationFailure::Operation),
        }
    }
}
