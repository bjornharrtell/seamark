//! Typed SeaORM mutation handlers for Atomic Operations.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    IntoActiveModel, ModelTrait, QueryFilter, QueryOrder, RuntimeErr, TransactionTrait, Value,
    sea_query::{Condition, Expr},
};
use serde_json::{Value as JsonValue, json};

use crate::atomic::{
    AtomicOperationFailure, AtomicOperationHandler, AtomicOperationOutcome,
    AtomicResourceChangeset, AtomicResourceData, AtomicResourceReference, AtomicResult,
    AtomicTarget, LocalIdMap, MappedRelationshipChange, PlannedOperation,
};
use crate::document::{Relationship, RelationshipData, ResourceIdentifier};
use crate::http::{
    AdapterResource, MutationAdapterError, MutationCommand, MutationOutcome,
    MutationResourceAdapter, RelationshipMutation, ResourceMutationChangeset,
};
use crate::registry::{
    RelationshipCardinality, RelationshipMapping, RelationshipPermission, RelationshipReassignment,
    RelationshipStorage, ResourceDefinition, ResourcePermission, ResourceRegistry,
};
use crate::seaorm::{
    SeaOrmComputedAttribute, SeaOrmMutationValueCodec, map_registered_model_with_computed,
    validate_computed_attributes,
};

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

type ResolvedBaseMutationCommand<'a> = (
    &'a dyn SeaOrmBaseMutationExecutor,
    MutationCommand,
    Vec<(RelationshipMapping, RelationshipData)>,
);

/// Runs each base HTTP mutation through one SeaORM transaction.
///
/// Exactly one matching typed executor handles a command. Resource create and
/// update commands with to-many linkage can be composed from a resource
/// executor and relationship executors in the same transaction. The adapter
/// does not translate base HTTP commands into Atomic Operations.
pub struct SeaOrmBaseMutationAdapter {
    database: DatabaseConnection,
    executors: Vec<Arc<dyn SeaOrmBaseMutationExecutor>>,
}

impl SeaOrmBaseMutationAdapter {
    /// Creates an adapter with explicit typed mutation executors.
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

impl SeaOrmBaseMutationAdapter {
    fn resolve_command<'a>(
        &'a self,
        resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<ResolvedBaseMutationCommand<'a>, MutationAdapterError> {
        let mut matching = self
            .executors
            .iter()
            .filter(|executor| executor.supports(resource, command));
        if let Some(executor) = matching.next() {
            if matching.next().is_some() {
                return Err(MutationAdapterError::Failed);
            }
            return Ok((executor.as_ref(), command.clone(), Vec::new()));
        }

        let Some((stripped_command, relationships)) = split_to_many_linkage(resource, command)
        else {
            return Err(MutationAdapterError::Unsupported);
        };
        let mut matching = self
            .executors
            .iter()
            .filter(|executor| executor.supports(resource, &stripped_command));
        let executor = matching.next().ok_or(MutationAdapterError::Unsupported)?;
        if matching.next().is_some() {
            return Err(MutationAdapterError::Failed);
        }
        Ok((executor.as_ref(), stripped_command, relationships))
    }

    fn validate_command(
        &self,
        resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<(), String> {
        let (_, _, relationships) = self
            .resolve_command(resource, command)
            .map_err(|error| format!("{error:?}"))?;
        for (relationship, data) in relationships {
            let relationship_command = MutationCommand::ModifyRelationship {
                id: "configuration-check".to_owned(),
                relationship: relationship.clone(),
                mutation: RelationshipMutation::Replace(data),
            };
            let mut matching = self
                .executors
                .iter()
                .filter(|executor| executor.supports(resource, &relationship_command));
            if matching.next().is_none() {
                return Err(format!(
                    "no relationship executor handles `{}` on `{}`",
                    relationship.public_name(),
                    resource.type_name()
                ));
            }
            if matching.next().is_some() {
                return Err(format!(
                    "multiple relationship executors handle `{}` on `{}`",
                    relationship.public_name(),
                    resource.type_name()
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MutationResourceAdapter for SeaOrmBaseMutationAdapter {
    async fn execute(
        &self,
        resource: &ResourceDefinition,
        command: MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        let (executor, dispatched_command, composed_relationships) =
            self.resolve_command(resource, &command)?;
        let transaction = self
            .database
            .begin()
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
        let execution = executor
            .execute(&transaction, resource, &dispatched_command)
            .await
            .map(|outcome| (outcome, composed_relationships));
        match execution {
            Ok((MutationOutcome::Resource(mut record), relationships))
                if !relationships.is_empty() =>
            {
                if record.id.is_empty() {
                    transaction
                        .rollback()
                        .await
                        .map_err(|_| MutationAdapterError::Failed)?;
                    return Err(MutationAdapterError::Failed);
                }
                let id = record.id.clone();
                let mut failure = None;
                for (relationship, data) in relationships {
                    let relationship_command = MutationCommand::ModifyRelationship {
                        id: id.clone(),
                        relationship: relationship.clone(),
                        mutation: RelationshipMutation::Replace(data),
                    };
                    let mut matching = self
                        .executors
                        .iter()
                        .filter(|executor| executor.supports(resource, &relationship_command));
                    let Some(relationship_executor) = matching.next() else {
                        failure = Some(MutationAdapterError::Unsupported);
                        break;
                    };
                    if matching.next().is_some() {
                        failure = Some(MutationAdapterError::Failed);
                        break;
                    }
                    match relationship_executor
                        .execute(&transaction, resource, &relationship_command)
                        .await
                    {
                        Ok(MutationOutcome::Relationship(data)) => {
                            record.relationships.insert(
                                relationship.model_field().to_owned(),
                                Relationship {
                                    data: Some(data),
                                    ..Relationship::default()
                                },
                            );
                        }
                        Ok(_) => {
                            failure = Some(MutationAdapterError::Failed);
                            break;
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                if let Some(error) = failure {
                    transaction
                        .rollback()
                        .await
                        .map_err(|_| MutationAdapterError::Failed)?;
                    Err(error)
                } else {
                    transaction
                        .commit()
                        .await
                        .map_err(|_| MutationAdapterError::Failed)?;
                    Ok(MutationOutcome::Resource(record))
                }
            }
            Ok((MutationOutcome::Resource(record), _)) if record.id.is_empty() => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| MutationAdapterError::Failed)?;
                Err(MutationAdapterError::Failed)
            }
            Ok((outcome, _)) => {
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

    fn validate_registry(&self, registry: &ResourceRegistry) -> Result<(), String> {
        for resource in registry.resources() {
            if resource.allows(ResourcePermission::Create) {
                self.validate_command(
                    resource,
                    &MutationCommand::Create {
                        changeset: Default::default(),
                    },
                )
                .map_err(|error| format!("resource `{}` create: {error}", resource.type_name()))?;
            }
            if resource.allows(ResourcePermission::Update) {
                self.validate_command(
                    resource,
                    &MutationCommand::Update {
                        id: "configuration-check".to_owned(),
                        changeset: Default::default(),
                    },
                )
                .map_err(|error| format!("resource `{}` update: {error}", resource.type_name()))?;
            }
            if resource.allows(ResourcePermission::Delete) {
                self.validate_command(
                    resource,
                    &MutationCommand::Delete {
                        id: "configuration-check".to_owned(),
                    },
                )
                .map_err(|error| format!("resource `{}` delete: {error}", resource.type_name()))?;
            }
            for relationship in resource.relationships() {
                if relationship.allows(RelationshipPermission::LinkageRead) {
                    self.validate_command(
                        resource,
                        &MutationCommand::ReadRelationship {
                            id: "configuration-check".to_owned(),
                            relationship: relationship.clone(),
                        },
                    )
                    .map_err(|error| {
                        format!(
                            "relationship `{}` on `{}` linkage read: {error}",
                            relationship.public_name(),
                            resource.type_name()
                        )
                    })?;
                }
                let cardinality = relationship.cardinality();
                if relationship.allows(RelationshipPermission::BaseReplace) {
                    let data = sample_relationship_data(relationship);
                    self.validate_command(
                        resource,
                        &MutationCommand::ModifyRelationship {
                            id: "configuration-check".to_owned(),
                            relationship: relationship.clone(),
                            mutation: RelationshipMutation::Replace(data),
                        },
                    )
                    .map_err(|error| {
                        format!(
                            "relationship `{}` on `{}` replace: {error}",
                            relationship.public_name(),
                            resource.type_name()
                        )
                    })?;
                }
                if cardinality == Some(RelationshipCardinality::ToMany) {
                    for (permission, mutation) in [
                        (
                            RelationshipPermission::BaseAdd,
                            RelationshipMutation::Add(Vec::new()),
                        ),
                        (
                            RelationshipPermission::BaseRemove,
                            RelationshipMutation::Remove(Vec::new()),
                        ),
                    ] {
                        if relationship.allows(permission) {
                            self.validate_command(
                                resource,
                                &MutationCommand::ModifyRelationship {
                                    id: "configuration-check".to_owned(),
                                    relationship: relationship.clone(),
                                    mutation,
                                },
                            )
                            .map_err(|error| {
                                format!(
                                    "relationship `{}` on `{}`: {error}",
                                    relationship.public_name(),
                                    resource.type_name()
                                )
                            })?;
                        }
                    }
                }
                if resource.allows(ResourcePermission::Create)
                    && relationship.allows(RelationshipPermission::ResourceCreate)
                {
                    self.validate_command(
                        resource,
                        &resource_command_with_relationship(true, relationship),
                    )
                    .map_err(|error| {
                        format!(
                            "resource `{}` create relationship `{}`: {error}",
                            resource.type_name(),
                            relationship.public_name()
                        )
                    })?;
                }
                if resource.allows(ResourcePermission::Update)
                    && relationship.allows(RelationshipPermission::ResourceUpdate)
                {
                    self.validate_command(
                        resource,
                        &resource_command_with_relationship(false, relationship),
                    )
                    .map_err(|error| {
                        format!(
                            "resource `{}` update relationship `{}`: {error}",
                            resource.type_name(),
                            relationship.public_name()
                        )
                    })?;
                }
            }
        }
        Ok(())
    }
}

fn sample_relationship_data(
    relationship: &crate::registry::RelationshipMapping,
) -> RelationshipData {
    match relationship.cardinality() {
        Some(RelationshipCardinality::ToMany) => RelationshipData::Many(Vec::new()),
        Some(RelationshipCardinality::ToOne) | None => RelationshipData::Null,
    }
}

fn resource_command_with_relationship(
    create: bool,
    relationship: &crate::registry::RelationshipMapping,
) -> MutationCommand {
    let data = sample_relationship_data(relationship);
    let changeset = crate::http::ResourceMutationChangeset {
        relationships: BTreeMap::from([(relationship.model_field().to_owned(), data)]),
        ..crate::http::ResourceMutationChangeset::default()
    };
    if create {
        MutationCommand::Create { changeset }
    } else {
        MutationCommand::Update {
            id: "configuration-check".to_owned(),
            changeset,
        }
    }
}

fn split_to_many_linkage(
    resource: &ResourceDefinition,
    command: &MutationCommand,
) -> Option<(
    MutationCommand,
    Vec<(crate::registry::RelationshipMapping, RelationshipData)>,
)> {
    let changeset = match command {
        MutationCommand::Create { changeset } | MutationCommand::Update { changeset, .. } => {
            changeset
        }
        _ => return None,
    };
    let mut remaining = changeset.clone();
    let mut relationships = Vec::new();
    for mapping in resource.relationships() {
        if mapping.cardinality() != Some(RelationshipCardinality::ToMany) {
            continue;
        }
        if let Some(data) = remaining.relationships.remove(mapping.model_field()) {
            relationships.push((mapping.clone(), data));
        }
    }
    if relationships.is_empty() {
        return None;
    }
    let command = match command {
        MutationCommand::Create { .. } => MutationCommand::Create {
            changeset: remaining,
        },
        MutationCommand::Update { id, .. } => MutationCommand::Update {
            id: id.clone(),
            changeset: remaining,
        },
        _ => unreachable!("command kind was checked above"),
    };
    Some((command, relationships))
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
    /// Creates a dispatcher whose operations must have one unambiguous executor.
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
            let executor =
                unique_atomic_executor(&self.executors, &operation)?.ok_or_else(|| {
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
        let executor = unique_atomic_executor(&self.executors, &operation)?.ok_or_else(|| {
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
        let executor = unique_atomic_executor(&self.executors, &operation)?.ok_or_else(|| {
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
    fn validate_registry(&self, registry: &ResourceRegistry) -> Result<(), String> {
        for resource in registry.resources() {
            let reference = AtomicResourceReference {
                type_name: resource.type_name().to_owned(),
                id: Some("configuration-check".to_owned()),
                lid: None,
                relationship: None,
            };
            let changeset = |relationships| AtomicResourceChangeset {
                type_name: resource.type_name().to_owned(),
                identifier_field: resource.identifier_field().to_owned(),
                id: None,
                lid: None,
                attributes: None,
                relationships,
            };
            if resource.allows(ResourcePermission::AtomicCreate) {
                self.validate_operation(&PlannedOperation::AddResource {
                    href: None,
                    data: atomic_resource_data(resource.type_name(), None),
                    changeset: changeset(None),
                })?;
            }
            if resource.allows(ResourcePermission::AtomicUpdate) {
                self.validate_operation(&PlannedOperation::UpdateResource {
                    target: AtomicTarget::Reference(reference.clone()),
                    data: atomic_resource_data(resource.type_name(), Some("configuration-check")),
                    changeset: changeset(None),
                })?;
            }
            if resource.allows(ResourcePermission::AtomicDelete) {
                self.validate_operation(&PlannedOperation::RemoveResource {
                    target: AtomicTarget::Reference(reference.clone()),
                })?;
            }
            for relationship in resource.relationships() {
                if relationship.allows(RelationshipPermission::AtomicReplace) {
                    self.validate_operation(&PlannedOperation::UpdateRelationship {
                        reference: relationship_reference(&reference, relationship),
                        model_field: relationship.model_field().to_owned(),
                        data: sample_relationship_data(relationship),
                    })?;
                }
                if relationship.cardinality() == Some(RelationshipCardinality::ToMany) {
                    if relationship.allows(RelationshipPermission::AtomicAdd) {
                        self.validate_operation(&PlannedOperation::AddRelationshipMembers {
                            reference: relationship_reference(&reference, relationship),
                            model_field: relationship.model_field().to_owned(),
                            data: Vec::new(),
                        })?;
                    }
                    if relationship.allows(RelationshipPermission::AtomicRemove) {
                        self.validate_operation(&PlannedOperation::RemoveRelationshipMembers {
                            reference: relationship_reference(&reference, relationship),
                            model_field: relationship.model_field().to_owned(),
                            data: Vec::new(),
                        })?;
                    }
                }
                if resource.allows(ResourcePermission::AtomicCreate)
                    && relationship.allows(RelationshipPermission::AtomicResourceCreate)
                {
                    let relationships = Some(BTreeMap::from([(
                        relationship.model_field().to_owned(),
                        MappedRelationshipChange {
                            data: Some(sample_relationship_data(relationship)),
                        },
                    )]));
                    self.validate_operation(&PlannedOperation::AddResource {
                        href: None,
                        data: atomic_resource_data(resource.type_name(), None),
                        changeset: changeset(relationships),
                    })?;
                }
                if resource.allows(ResourcePermission::AtomicUpdate)
                    && relationship.allows(RelationshipPermission::AtomicResourceUpdate)
                {
                    let relationships = Some(BTreeMap::from([(
                        relationship.model_field().to_owned(),
                        MappedRelationshipChange {
                            data: Some(sample_relationship_data(relationship)),
                        },
                    )]));
                    self.validate_operation(&PlannedOperation::UpdateResource {
                        target: AtomicTarget::Reference(reference.clone()),
                        data: atomic_resource_data(
                            resource.type_name(),
                            Some("configuration-check"),
                        ),
                        changeset: changeset(relationships),
                    })?;
                }
            }
        }
        Ok(())
    }

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
        if let Some(executor) = unique_atomic_executor(&self.executors, operation)? {
            return executor
                .execute_with_failure(transaction, operation, local_ids)
                .await;
        }
        if let PlannedOperation::AddResource {
            href,
            data,
            changeset,
        } = operation
            && has_to_many_relationships(changeset)
        {
            return self
                .execute_composed_resource_add(transaction, href, data, changeset, local_ids)
                .await;
        }
        if let PlannedOperation::UpdateResource {
            target: AtomicTarget::Reference(reference),
            data,
            changeset,
        } = operation
            && has_to_many_relationships(changeset)
        {
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
        Err(AtomicOperationFailure::Operation(
            "no SeaORM mutation executor supports this operation".to_owned(),
        ))
    }
}

impl SeaOrmAtomicOperationDispatcher {
    fn validate_operation(&self, operation: &PlannedOperation) -> Result<(), String> {
        if unique_atomic_executor(&self.executors, operation)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        let (reference, changeset) = match operation {
            PlannedOperation::AddResource {
                data, changeset, ..
            } if has_to_many_relationships(changeset) => {
                let (_, stripped) = without_to_many_relationships(data, changeset);
                let resource_operation = PlannedOperation::AddResource {
                    href: None,
                    data: data.clone(),
                    changeset: stripped,
                };
                if unique_atomic_executor(&self.executors, &resource_operation)
                    .map_err(|error| error.to_string())?
                    .is_none()
                {
                    return Err(format!(
                        "no resource executor supports Atomic create for `{}`",
                        changeset.type_name
                    ));
                }
                (
                    AtomicResourceReference {
                        type_name: changeset.type_name.clone(),
                        id: Some("configuration-check".to_owned()),
                        lid: None,
                        relationship: None,
                    },
                    changeset,
                )
            }
            PlannedOperation::UpdateResource {
                target: AtomicTarget::Reference(reference),
                data,
                changeset,
            } if has_to_many_relationships(changeset) => {
                let (_, stripped) = without_to_many_relationships(data, changeset);
                let resource_operation = PlannedOperation::UpdateResource {
                    target: AtomicTarget::Reference(reference.clone()),
                    data: data.clone(),
                    changeset: stripped,
                };
                if unique_atomic_executor(&self.executors, &resource_operation)
                    .map_err(|error| error.to_string())?
                    .is_none()
                {
                    return Err(format!(
                        "no resource executor supports Atomic update for `{}`",
                        changeset.type_name
                    ));
                }
                (reference.clone(), changeset)
            }
            _ => {
                return Err("no SeaORM executor supports an enabled Atomic operation".to_owned());
            }
        };
        for (model_field, change) in changeset.relationships.as_ref().into_iter().flatten() {
            let Some(RelationshipData::Many(data)) = &change.data else {
                continue;
            };
            let relationship_operation = PlannedOperation::UpdateRelationship {
                reference: reference.clone(),
                model_field: model_field.clone(),
                data: RelationshipData::Many(data.clone()),
            };
            if unique_atomic_executor(&self.executors, &relationship_operation)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err(format!(
                    "no relationship executor supports Atomic to-many field `{model_field}`"
                ));
            }
        }
        Ok(())
    }
}

fn atomic_resource_data(type_name: &str, id: Option<&str>) -> AtomicResourceData {
    AtomicResourceData {
        type_name: type_name.to_owned(),
        id: id.map(str::to_owned),
        lid: None,
        attributes: None,
        relationships: None,
        links: None,
        meta: None,
    }
}

fn relationship_reference(
    source: &AtomicResourceReference,
    relationship: &crate::registry::RelationshipMapping,
) -> AtomicResourceReference {
    AtomicResourceReference {
        type_name: source.type_name.clone(),
        id: source.id.clone(),
        lid: source.lid.clone(),
        relationship: Some(relationship.public_name().to_owned()),
    }
}

fn unique_atomic_executor<'a>(
    executors: &'a [Arc<dyn SeaOrmAtomicOperationExecutor>],
    operation: &PlannedOperation,
) -> Result<Option<&'a dyn SeaOrmAtomicOperationExecutor>, AtomicOperationFailure> {
    let mut matching = executors
        .iter()
        .filter(|executor| executor.supports(operation));
    let Some(first) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(AtomicOperationFailure::Operation(
            "multiple SeaORM mutation executors support the same operation".to_owned(),
        ));
    }
    Ok(Some(first.as_ref()))
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

/// Populates additional required columns on an inserted join-table row.
type JoinTableInsertColumns<A> = Arc<dyn Fn(&mut A) -> Result<(), String> + Send + Sync>;

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
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send + 'static,
    E::Model: IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    source_type: String,
    model_field: String,
    target_type: String,
    source_column: String,
    target_column: String,
    position_column: Option<String>,
    value_codec: C,
    insert_columns: Option<JoinTableInsertColumns<E::ActiveModel>>,
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
        value_codec: C,
    ) -> Result<Self, String> {
        Self::new_with_insert_columns(registry, source_type, relationship_name, value_codec, None)
    }

    /// Creates a typed join-table executor that populates additional required
    /// columns on every inserted membership row.
    ///
    /// `insert_columns` is called on each new `ActiveModel` after the source
    /// and target columns are set and before the row is inserted. Use it for
    /// join tables with extra non-nullable columns.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource or relationship is not registered or
    /// either join-table column is not present on the typed entity.
    pub fn new_with_insert_columns(
        registry: &ResourceRegistry,
        source_type: &str,
        relationship_name: &str,
        value_codec: C,
        insert_columns: Option<JoinTableInsertColumns<E::ActiveModel>>,
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
        let (source_column, target_column, position_column) = match relationship.storage() {
            RelationshipStorage::JoinTable {
                source_column,
                target_column,
            } => (source_column.clone(), target_column.clone(), None),
            RelationshipStorage::OrderedJoinTable {
                source_column,
                target_column,
                position_column,
            } => (
                source_column.clone(),
                target_column.clone(),
                Some(position_column.clone()),
            ),
            _ => {
                return Err(format!(
                    "relationship `{source_type}.{relationship_name}` is not mapped through a join table"
                ));
            }
        };
        E::Column::from_str(&source_column)
            .map_err(|_| format!("join-table field `{source_column}` is not a SeaORM column"))?;
        E::Column::from_str(&target_column)
            .map_err(|_| format!("join-table field `{target_column}` is not a SeaORM column"))?;
        if let Some(position_column) = &position_column {
            E::Column::from_str(position_column).map_err(|_| {
                format!("join-table position field `{position_column}` is not a SeaORM column")
            })?;
        }

        Ok(Self {
            source_type: source_type.to_owned(),
            model_field: relationship.model_field().to_owned(),
            target_type: target.type_name().to_owned(),
            source_column,
            target_column,
            position_column,
            value_codec,
            insert_columns,
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
        self.value_codec.encode_identifier(model_field, identifier)
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
                .map_err(|_| "join-table replacement delete failed".to_owned())?;
        } else if matches!(action, JoinTableOperation::Remove) && !target_values.is_empty() {
            E::delete_many()
                .filter(source_column.eq(source_value.clone()))
                .filter(target_column.is_in(target_values.clone()))
                .exec(transaction)
                .await
                .map_err(|_| "join-table delete failed".to_owned())?;
        }

        let position_column = match &self.position_column {
            Some(name) => Some(
                E::Column::from_str(name)
                    .map_err(|_| "join-table position column is not a SeaORM column".to_owned())?,
            ),
            None => None,
        };
        let mut next_position = 0_i32;
        if position_column.is_some() && matches!(action, JoinTableOperation::Add) {
            let existing = E::find()
                .filter(source_column.eq(source_value.clone()))
                .all(transaction)
                .await
                .map_err(|_| "join-table position lookup failed".to_owned())?
                .len();
            next_position = i32::try_from(existing).unwrap_or(i32::MAX);
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
                        .map_err(|_| "join-table membership lookup failed".to_owned())?
                        .is_some()
                {
                    continue;
                }
                let mut active_model = <E::ActiveModel as Default>::default();
                active_model
                    .try_set(source_column, source_value.clone())
                    .map_err(|_| "could not map join-table source column".to_owned())?;
                active_model
                    .try_set(target_column, target_value)
                    .map_err(|_| "could not map join-table target column".to_owned())?;
                if let Some(insert_columns) = &self.insert_columns {
                    insert_columns(&mut active_model)?;
                }
                if let Some(position_column) = position_column {
                    active_model
                        .try_set(position_column, next_position.into())
                        .map_err(|_| "could not map join-table position column".to_owned())?;
                    next_position = next_position.saturating_add(1);
                }
                active_model.insert(transaction).await.map_err(|error| {
                    if is_foreign_key_violation(&error) {
                        AtomicOperationFailure::NotFound(
                            "a referenced relationship resource does not exist".to_owned(),
                        )
                    } else {
                        AtomicOperationFailure::Operation("join-table insert failed".to_owned())
                    }
                })?;
            }
        }

        Ok(AtomicOperationOutcome::default())
    }
}

#[async_trait]
impl<E, C> SeaOrmBaseMutationExecutor for SeaOrmJoinTableMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec + Send + Sync,
{
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool {
        if resource.type_name() != self.source_type {
            return false;
        }
        match command {
            MutationCommand::ReadRelationship { relationship, .. }
            | MutationCommand::ModifyRelationship { relationship, .. } => {
                relationship.model_field() == self.model_field
                    && relationship.target_type() == self.target_type
                    && relationship.cardinality() == Some(RelationshipCardinality::ToMany)
            }
            _ => false,
        }
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        _resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        match command {
            MutationCommand::ReadRelationship { id, relationship } => {
                Ok(MutationOutcome::Relationship(
                    self.read_linkage(transaction, id, relationship).await?,
                ))
            }
            MutationCommand::ModifyRelationship {
                id,
                relationship,
                mutation,
            } => {
                let reference = AtomicResourceReference {
                    type_name: self.source_type.clone(),
                    id: Some(id.clone()),
                    lid: None,
                    relationship: Some(relationship.public_name().to_owned()),
                };
                let operation = match mutation {
                    RelationshipMutation::Replace(RelationshipData::Many(data)) => {
                        PlannedOperation::UpdateRelationship {
                            reference,
                            model_field: self.model_field.clone(),
                            data: RelationshipData::Many(data.clone()),
                        }
                    }
                    RelationshipMutation::Add(data) => PlannedOperation::AddRelationshipMembers {
                        reference,
                        model_field: self.model_field.clone(),
                        data: data.clone(),
                    },
                    RelationshipMutation::Remove(data) => {
                        PlannedOperation::RemoveRelationshipMembers {
                            reference,
                            model_field: self.model_field.clone(),
                            data: data.clone(),
                        }
                    }
                    RelationshipMutation::Replace(_) => {
                        return Err(MutationAdapterError::Unsupported);
                    }
                };
                SeaOrmAtomicOperationExecutor::execute_with_failure(
                    self,
                    transaction,
                    &operation,
                    &LocalIdMap::default(),
                )
                .await
                .map_err(base_mutation_failure)?;
                Ok(MutationOutcome::Relationship(
                    self.read_linkage(transaction, id, relationship).await?,
                ))
            }
            _ => Err(MutationAdapterError::Unsupported),
        }
    }
}

impl<E, C> SeaOrmJoinTableMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    async fn read_linkage(
        &self,
        transaction: &DatabaseTransaction,
        source_id: &str,
        relationship: &crate::registry::RelationshipMapping,
    ) -> Result<RelationshipData, MutationAdapterError> {
        let source_column =
            E::Column::from_str(&self.source_column).map_err(|_| MutationAdapterError::Failed)?;
        let target_column =
            E::Column::from_str(&self.target_column).map_err(|_| MutationAdapterError::Failed)?;
        let source_value = self
            .encode_identifier(&self.source_column, source_id)
            .map_err(|_| MutationAdapterError::Failed)?;
        let mut select = E::find().filter(source_column.eq(source_value));
        if let Some(position_column) = &self.position_column {
            let position_column =
                E::Column::from_str(position_column).map_err(|_| MutationAdapterError::Failed)?;
            select = select.order_by_asc(position_column);
        }
        let links = select
            .all(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
        let identifiers = links
            .iter()
            .map(|link| {
                self.value_codec
                    .decode_identifier(&self.target_column, &link.get(target_column))
                    .map(|id| ResourceIdentifier {
                        type_name: relationship.target_type().to_owned(),
                        id: Some(id),
                        ..ResourceIdentifier::default()
                    })
                    .map_err(|_| MutationAdapterError::Failed)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RelationshipData::Many(identifiers))
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
    nullable: bool,
    reassignment: RelationshipReassignment,
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
    /// # Errors
    ///
    /// Returns an error if the source resource or relationship is not
    /// registered, or either target entity column is unknown.
    pub fn new(
        registry: &ResourceRegistry,
        source_type: &str,
        relationship_name: &str,
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
        let RelationshipStorage::ToManyForeignKey {
            foreign_key_field,
            nullable,
            reassignment,
        } = relationship.storage()
        else {
            return Err(format!(
                "relationship `{source_type}.{relationship_name}` is not mapped through a target foreign key"
            ));
        };
        let foreign_key_field = foreign_key_field.clone();
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
            nullable: *nullable,
            reassignment: *reassignment,
            value_codec,
            entity: PhantomData,
        })
    }

    fn supports_relationship_operation(&self, operation: &PlannedOperation) -> bool {
        let owned_relationship = |reference: &AtomicResourceReference, model_field: &String| {
            reference.type_name == self.source_type && model_field == &self.model_field
        };
        match operation {
            PlannedOperation::AddRelationshipMembers {
                reference,
                model_field,
                ..
            } if owned_relationship(reference, model_field) => true,
            PlannedOperation::RemoveRelationshipMembers {
                reference,
                model_field,
                ..
            }
            | PlannedOperation::UpdateRelationship {
                reference,
                model_field,
                data: RelationshipData::Many(_),
                ..
            } if owned_relationship(reference, model_field) => self.nullable,
            _ => false,
        }
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
        let source_value = self
            .value_codec
            .encode_identifier(&self.foreign_key_field, &source_id)?;
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
                self.value_codec
                    .encode_identifier(&self.target_identifier_field, &id)
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
                .map_err(|_| "to-many foreign-key replacement clear failed".to_owned())?;
        }

        for target_value in target_values {
            if matches!(action, ForeignKeyOperation::Add)
                && E::find()
                    .filter(target_identifier_column.eq(target_value.clone()))
                    .filter(foreign_key_column.eq(source_value.clone()))
                    .one(transaction)
                    .await
                    .map_err(|_| "to-many foreign-key membership lookup failed".to_owned())?
                    .is_some()
            {
                continue;
            }
            let query = E::update_many().filter(target_identifier_column.eq(target_value.clone()));
            let query = match action {
                ForeignKeyOperation::Add | ForeignKeyOperation::Replace
                    if self.reassignment == RelationshipReassignment::Deny =>
                {
                    query.filter(
                        Condition::any()
                            .add(foreign_key_column.is_null())
                            .add(foreign_key_column.eq(source_value.clone())),
                    )
                }
                ForeignKeyOperation::Remove => {
                    query.filter(Condition::all().add(foreign_key_column.eq(source_value.clone())))
                }
                _ => query,
            };
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
                .map_err(|_| "to-many foreign-key update failed".to_owned())?;
            if matches!(
                action,
                ForeignKeyOperation::Add | ForeignKeyOperation::Replace
            ) && result.rows_affected == 0
            {
                let target_exists = E::find()
                    .filter(target_identifier_column.eq(target_value.clone()))
                    .one(transaction)
                    .await
                    .map_err(|_| "to-many foreign-key target lookup failed".to_owned())?
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

#[async_trait]
impl<E, C> SeaOrmBaseMutationExecutor for SeaOrmToManyForeignKeyMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec + Send + Sync,
{
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool {
        resource.type_name() == self.source_type
            && match command {
                MutationCommand::ReadRelationship { relationship, .. }
                | MutationCommand::ModifyRelationship { relationship, .. } => {
                    relationship.model_field() == self.model_field
                        && relationship.target_type() == self.target_type
                        && relationship.cardinality() == Some(RelationshipCardinality::ToMany)
                }
                _ => false,
            }
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        _resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        match command {
            MutationCommand::ReadRelationship { id, relationship } => {
                Ok(MutationOutcome::Relationship(
                    self.read_linkage(transaction, id, relationship).await?,
                ))
            }
            MutationCommand::ModifyRelationship {
                id,
                relationship,
                mutation,
            } => {
                let reference = AtomicResourceReference {
                    type_name: self.source_type.clone(),
                    id: Some(id.clone()),
                    lid: None,
                    relationship: Some(relationship.public_name().to_owned()),
                };
                let operation = match mutation {
                    RelationshipMutation::Replace(RelationshipData::Many(data)) => {
                        PlannedOperation::UpdateRelationship {
                            reference,
                            model_field: self.model_field.clone(),
                            data: RelationshipData::Many(data.clone()),
                        }
                    }
                    RelationshipMutation::Add(data) => PlannedOperation::AddRelationshipMembers {
                        reference,
                        model_field: self.model_field.clone(),
                        data: data.clone(),
                    },
                    RelationshipMutation::Remove(data) => {
                        PlannedOperation::RemoveRelationshipMembers {
                            reference,
                            model_field: self.model_field.clone(),
                            data: data.clone(),
                        }
                    }
                    RelationshipMutation::Replace(_) => {
                        return Err(MutationAdapterError::Unsupported);
                    }
                };
                SeaOrmAtomicOperationExecutor::execute_with_failure(
                    self,
                    transaction,
                    &operation,
                    &LocalIdMap::default(),
                )
                .await
                .map_err(base_mutation_failure)?;
                Ok(MutationOutcome::Relationship(
                    self.read_linkage(transaction, id, relationship).await?,
                ))
            }
            _ => Err(MutationAdapterError::Unsupported),
        }
    }
}

impl<E, C> SeaOrmToManyForeignKeyMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    E::Column: ColumnTrait + FromStr,
    C: SeaOrmMutationValueCodec,
{
    async fn read_linkage(
        &self,
        transaction: &DatabaseTransaction,
        source_id: &str,
        relationship: &crate::registry::RelationshipMapping,
    ) -> Result<RelationshipData, MutationAdapterError> {
        let source_column = E::Column::from_str(&self.foreign_key_field)
            .map_err(|_| MutationAdapterError::Failed)?;
        let target_column = E::Column::from_str(&self.target_identifier_field)
            .map_err(|_| MutationAdapterError::Failed)?;
        let source_value = self
            .value_codec
            .encode_identifier(&self.foreign_key_field, source_id)
            .map_err(|_| MutationAdapterError::Failed)?;
        let members = E::find()
            .filter(source_column.eq(source_value))
            .all(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?;
        let identifiers = members
            .iter()
            .map(|member| {
                self.value_codec
                    .decode_identifier(&self.target_identifier_field, &member.get(target_column))
                    .map(|id| ResourceIdentifier {
                        type_name: relationship.target_type().to_owned(),
                        id: Some(id),
                        ..ResourceIdentifier::default()
                    })
                    .map_err(|_| MutationAdapterError::Failed)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RelationshipData::Many(identifiers))
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
/// implementations in the dispatcher. Resource updates return no result
/// representation; if entity hooks or database triggers change additional
/// public fields, a higher-priority custom executor must return the updated
/// representation required by the Atomic Operations extension.
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
    computed_attributes: Vec<SeaOrmComputedAttribute<E>>,
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
    ) -> Result<Self, String> {
        Self::new_with_computed_attributes(registry, resource_type, value_codec, Vec::new())
    }

    /// Binds this typed executor to a registered public resource and its
    /// computed read-only attributes.
    ///
    /// Each computed mapping must also be registered on the resource and may
    /// not be filtered, sorted, or written. Computed fields are included in
    /// the resource representations returned after ordinary mutations.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource type, entity columns, or computed
    /// attribute mappings are invalid.
    pub fn new_with_computed_attributes(
        registry: &ResourceRegistry,
        resource_type: &str,
        value_codec: C,
        computed_attributes: Vec<SeaOrmComputedAttribute<E>>,
    ) -> Result<Self, String> {
        let definition = registry
            .resource(resource_type)
            .map_err(|error| error.to_string())?
            .clone();
        let computed_fields = validate_computed_attributes::<E>(&definition, &computed_attributes)?;
        E::Column::from_str(definition.identifier_field()).map_err(|_| {
            format!(
                "identifier field `{}` is not a SeaORM column",
                definition.identifier_field()
            )
        })?;
        for attribute in definition.attributes() {
            if !computed_fields.contains(attribute.model_field()) {
                E::Column::from_str(attribute.model_field()).map_err(|_| {
                    format!(
                        "attribute `{}` maps to non-column `{}`; register it as a computed attribute or use an application executor",
                        attribute.public_name(),
                        attribute.model_field()
                    )
                })?;
            }
        }
        for relationship in definition.relationships() {
            if relationship.cardinality() != Some(RelationshipCardinality::ToMany) {
                E::Column::from_str(relationship.model_field()).map_err(|_| {
                    format!(
                        "relationship `{}` maps to non-column `{}`",
                        relationship.public_name(),
                        relationship.model_field()
                    )
                })?;
            }
        }
        Ok(Self {
            definition,
            value_codec,
            computed_attributes,
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
            .map_err(|_| format!("could not map field `{model_field}`"))
    }

    fn set_identifier_field(
        &self,
        active_model: &mut E::ActiveModel,
        model_field: &str,
        identifier: &str,
    ) -> Result<(), String> {
        let column = self.column(model_field)?;
        let value = self
            .value_codec
            .encode_identifier(model_field, identifier)?;
        active_model
            .try_set(column, value)
            .map_err(|_| format!("could not map identifier field `{model_field}`"))
    }

    fn apply_changeset(
        &self,
        active_model: &mut E::ActiveModel,
        changeset: &AtomicResourceChangeset,
        local_ids: &LocalIdMap,
        include_identifier: bool,
    ) -> Result<(), String> {
        if include_identifier && let Some(id) = &changeset.id {
            self.set_identifier_field(active_model, &changeset.identifier_field, id)?;
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
                match data {
                    RelationshipData::Null => {
                        self.set_field(active_model, model_field, &JsonValue::Null)?;
                    }
                    RelationshipData::One(identifier) => {
                        let identifier = local_ids.resolve(identifier)?;
                        let id = identifier.id.ok_or_else(|| {
                            "resolved relationship identity has no persistent `id`".to_owned()
                        })?;
                        self.set_identifier_field(active_model, model_field, &id)?;
                    }
                    RelationshipData::Many(_) => {
                        return Err(format!(
                            "to-many relationship field `{model_field}` requires an application executor"
                        ));
                    }
                }
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
        self.value_codec
            .encode_identifier(self.definition.identifier_field(), id)
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
                AtomicOperationFailure::Operation("resource create failed".to_owned())
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
                .map_err(|_| {
                    AtomicOperationFailure::Operation("resource lookup failed".to_owned())
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
            .map_err(|_| "could not map identifier field".to_owned())?;
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
                AtomicOperationFailure::Operation("resource update failed".to_owned())
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
            .map_err(|_| AtomicOperationFailure::Operation("resource delete failed".to_owned()))?;
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
        let mut active_model = <E::ActiveModel as std::default::Default>::default();
        match data {
            RelationshipData::Null => {
                self.set_field(&mut active_model, model_field, &JsonValue::Null)?;
            }
            RelationshipData::One(identifier) => {
                let identifier = local_ids.resolve(identifier)?;
                let id = identifier.id.ok_or_else(|| {
                    "resolved relationship identity has no persistent `id`".to_owned()
                })?;
                self.set_identifier_field(&mut active_model, model_field, &id)?;
            }
            RelationshipData::Many(_) => {
                return Err(AtomicOperationFailure::Operation(format!(
                    "to-many relationship field `{model_field}` requires an application executor"
                )));
            }
        }

        let id_column = self.column(self.definition.identifier_field())?;
        let id_value = self.identifier_value(&id)?;
        active_model
            .try_set(id_column, id_value)
            .map_err(|_| "could not map identifier field".to_owned())?;
        active_model.update(transaction).await.map_err(|error| {
            if matches!(&error, DbErr::RecordNotUpdated) {
                AtomicOperationFailure::NotFound("relationship owner was not found".to_owned())
            } else if matches!(data, RelationshipData::One(_)) && is_foreign_key_violation(&error) {
                AtomicOperationFailure::NotFound(
                    "a referenced relationship resource does not exist".to_owned(),
                )
            } else {
                AtomicOperationFailure::Operation("relationship update failed".to_owned())
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
    E::Column: ColumnTrait + FromStr,
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
        match operation {
            PlannedOperation::AddResource { changeset, .. } => {
                let mut outcome = self.add(transaction, changeset, local_ids).await?;
                let id = outcome
                    .result
                    .data
                    .as_ref()
                    .and_then(|data| data.get("id"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                if let Some(id) = id {
                    outcome.result.data = Some(self.representation_result(transaction, &id).await?);
                }
                Ok(outcome)
            }
            PlannedOperation::UpdateResource {
                target, changeset, ..
            } => {
                let mut outcome = self
                    .update(transaction, target, changeset, local_ids)
                    .await?;
                let identity = self.target_identity(target, local_ids)?;
                if let Some(id) = identity.id {
                    outcome.result.data = Some(self.representation_result(transaction, &id).await?);
                }
                Ok(outcome)
            }
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
                Err(AtomicOperationFailure::Operation(
                    "to-many relationship operations require an application executor".to_owned(),
                ))
            }
        }
    }
}

#[async_trait]
impl<E, C> SeaOrmBaseMutationExecutor for SeaOrmResourceMutationHandler<E, C>
where
    E: EntityTrait + 'static,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Column: ColumnTrait + FromStr,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    C: SeaOrmMutationValueCodec + Send + Sync,
{
    fn supports(&self, resource: &ResourceDefinition, command: &MutationCommand) -> bool {
        if resource.type_name() != self.definition.type_name() {
            return false;
        }
        match command {
            MutationCommand::Create { changeset } => self.supports_base_changeset(changeset),
            MutationCommand::Update { changeset, .. } => self.supports_base_changeset(changeset),
            MutationCommand::Delete { .. } => true,
            MutationCommand::ReadRelationship { relationship, .. } => {
                relationship.cardinality() == Some(RelationshipCardinality::ToOne)
                    && E::Column::from_str(relationship.model_field()).is_ok()
            }
            MutationCommand::ModifyRelationship {
                relationship,
                mutation: RelationshipMutation::Replace(data),
                ..
            } => {
                relationship.cardinality() == Some(RelationshipCardinality::ToOne)
                    && matches!(data, RelationshipData::Null | RelationshipData::One(_))
                    && E::Column::from_str(relationship.model_field()).is_ok()
            }
            MutationCommand::ModifyRelationship { .. } => false,
        }
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        _resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        let local_ids = LocalIdMap::default();
        match command {
            MutationCommand::Create { changeset } => {
                let changeset = self.base_changeset(changeset, None);
                let result = self
                    .add(transaction, &changeset, &local_ids)
                    .await
                    .map_err(base_mutation_failure)?;
                let id = result
                    .result
                    .data
                    .as_ref()
                    .and_then(|data| data.get("id"))
                    .and_then(JsonValue::as_str)
                    .ok_or(MutationAdapterError::Failed)?;
                Ok(MutationOutcome::Resource(
                    self.load_adapter_resource(transaction, id).await?,
                ))
            }
            MutationCommand::Update { id, changeset } => {
                let changeset = self.base_changeset(changeset, Some(id));
                let target = AtomicTarget::Reference(AtomicResourceReference {
                    type_name: self.definition.type_name().to_owned(),
                    id: Some(id.clone()),
                    lid: None,
                    relationship: None,
                });
                self.update(transaction, &target, &changeset, &local_ids)
                    .await
                    .map_err(base_mutation_failure)?;
                Ok(MutationOutcome::Resource(
                    self.load_adapter_resource(transaction, id).await?,
                ))
            }
            MutationCommand::Delete { id } => {
                let target = AtomicTarget::Reference(AtomicResourceReference {
                    type_name: self.definition.type_name().to_owned(),
                    id: Some(id.clone()),
                    lid: None,
                    relationship: None,
                });
                self.remove(transaction, &target, &local_ids)
                    .await
                    .map_err(base_mutation_failure)?;
                Ok(MutationOutcome::Deleted)
            }
            MutationCommand::ReadRelationship { id, relationship } => {
                let resource = self.load_adapter_resource(transaction, id).await?;
                let relationship = resource
                    .relationships
                    .get(relationship.model_field())
                    .and_then(|relationship| relationship.data.clone())
                    .ok_or(MutationAdapterError::Failed)?;
                Ok(MutationOutcome::Relationship(relationship))
            }
            MutationCommand::ModifyRelationship {
                id,
                relationship,
                mutation: RelationshipMutation::Replace(data),
            } => {
                let changeset = AtomicResourceChangeset {
                    type_name: self.definition.type_name().to_owned(),
                    identifier_field: self.definition.identifier_field().to_owned(),
                    id: Some(id.clone()),
                    lid: None,
                    attributes: None,
                    relationships: Some(BTreeMap::from([(
                        relationship.model_field().to_owned(),
                        MappedRelationshipChange {
                            data: Some(data.clone()),
                        },
                    )])),
                };
                let target = AtomicTarget::Reference(AtomicResourceReference {
                    type_name: self.definition.type_name().to_owned(),
                    id: Some(id.clone()),
                    lid: None,
                    relationship: None,
                });
                self.update(transaction, &target, &changeset, &local_ids)
                    .await
                    .map_err(base_mutation_failure)?;
                let resource = self.load_adapter_resource(transaction, id).await?;
                let data = resource
                    .relationships
                    .get(relationship.model_field())
                    .and_then(|relationship| relationship.data.clone())
                    .ok_or(MutationAdapterError::Failed)?;
                Ok(MutationOutcome::Relationship(data))
            }
            MutationCommand::ModifyRelationship { .. } => Err(MutationAdapterError::Unsupported),
        }
    }
}

impl<E, C> SeaOrmResourceMutationHandler<E, C>
where
    E: EntityTrait,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Default + Send,
    E::Column: ColumnTrait + FromStr,
    E::Model: ModelTrait<Entity = E> + IntoActiveModel<E::ActiveModel> + Send,
    C: SeaOrmMutationValueCodec,
{
    fn supports_base_changeset(&self, changeset: &ResourceMutationChangeset) -> bool {
        changeset.relationships.keys().all(|field| {
            self.definition
                .relationships()
                .iter()
                .find(|relationship| relationship.model_field() == field)
                .is_some_and(|relationship| {
                    relationship.cardinality() == Some(RelationshipCardinality::ToOne)
                        && E::Column::from_str(field).is_ok()
                })
        })
    }

    fn base_changeset(
        &self,
        changeset: &ResourceMutationChangeset,
        id: Option<&str>,
    ) -> AtomicResourceChangeset {
        AtomicResourceChangeset {
            type_name: self.definition.type_name().to_owned(),
            identifier_field: self.definition.identifier_field().to_owned(),
            id: id.map(str::to_owned),
            lid: None,
            attributes: Some(changeset.attributes.clone()),
            relationships: Some(
                changeset
                    .relationships
                    .iter()
                    .map(|(field, data)| {
                        (
                            field.clone(),
                            MappedRelationshipChange {
                                data: Some(data.clone()),
                            },
                        )
                    })
                    .collect(),
            ),
        }
    }

    async fn load_adapter_resource(
        &self,
        transaction: &DatabaseTransaction,
        id: &str,
    ) -> Result<AdapterResource, MutationAdapterError> {
        let identifier = self
            .column(self.definition.identifier_field())
            .map_err(|_| MutationAdapterError::Failed)?;
        let value = self
            .identifier_value(id)
            .map_err(|_| MutationAdapterError::Failed)?;
        let model = E::find()
            .filter(identifier.eq(value))
            .one(transaction)
            .await
            .map_err(|_| MutationAdapterError::Failed)?
            .ok_or(MutationAdapterError::NotFound)?;
        map_registered_model_with_computed::<E>(&model, &self.definition, &self.computed_attributes)
            .map_err(|_| MutationAdapterError::Failed)
    }

    /// Loads the public representation returned by resource add/update results.
    ///
    /// To-many relationships are omitted: the standard mutation mapper cannot
    /// load their linkage, and omitting a field is preferable to reporting an
    /// inaccurate empty relationship.
    async fn representation_result(
        &self,
        transaction: &DatabaseTransaction,
        id: &str,
    ) -> Result<JsonValue, AtomicOperationFailure> {
        let mut resource = self
            .load_adapter_resource(transaction, id)
            .await
            .map_err(|_| {
                AtomicOperationFailure::Operation(
                    "could not load the resource representation".to_owned(),
                )
            })?;
        for relationship in self.definition.relationships() {
            if relationship.cardinality() == Some(RelationshipCardinality::ToMany) {
                resource.relationships.remove(relationship.model_field());
            }
        }
        let projected =
            crate::projection::project_resource(&self.definition, &resource).map_err(|_| {
                AtomicOperationFailure::Operation(
                    "could not project the resource representation".to_owned(),
                )
            })?;
        serde_json::to_value(projected).map_err(|_| {
            AtomicOperationFailure::Operation(
                "could not serialize the resource representation".to_owned(),
            )
        })
    }
}

fn base_mutation_failure(failure: AtomicOperationFailure) -> MutationAdapterError {
    match failure {
        AtomicOperationFailure::NotFound(_) => MutationAdapterError::NotFound,
        AtomicOperationFailure::Conflict(_) => MutationAdapterError::Conflict,
        AtomicOperationFailure::Operation(_) => MutationAdapterError::Failed,
    }
}
