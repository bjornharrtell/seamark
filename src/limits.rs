//! Reusable request limits for planned reads and mutations.

use crate::atomic::{AtomicOperationsGuard, PlannedAtomicOperation, PlannedOperation};
use crate::document::RelationshipData;
use crate::http::{
    MutationCommand, QueryAdapterError, QueryCollectionResult, QueryResourceAdapter,
    QueryResourceResult,
};
use crate::query::{FilterExpression, IncludeNode, ReadPlan};
use crate::registry::ResourceDefinition;
use async_trait::async_trait;
use axum::http::HeaderMap;
use std::sync::Arc;

/// Optional limits for include expansion, filters, linkage, and Atomic batches.
///
/// Each limit is disabled until configured. Apply the same value to the HTTP
/// router and the Atomic guard to enforce it at both entry points.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionLimits {
    max_include_depth: Option<usize>,
    max_include_relationships: Option<usize>,
    max_filter_nodes: Option<usize>,
    max_relationship_members: Option<usize>,
    max_atomic_operations: Option<usize>,
    max_included_resources: Option<usize>,
    max_include_queries: Option<usize>,
}

impl ExecutionLimits {
    /// Creates an empty set of limits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum include path depth. Zero disables all includes.
    #[must_use]
    pub fn max_include_depth(mut self, maximum: usize) -> Self {
        self.max_include_depth = Some(maximum);
        self
    }

    /// Sets the maximum number of relationship edges across an include tree.
    #[must_use]
    pub fn max_include_relationships(mut self, maximum: usize) -> Self {
        self.max_include_relationships = Some(maximum);
        self
    }

    /// Sets the maximum number of filter expression nodes in one request.
    #[must_use]
    pub fn max_filter_nodes(mut self, maximum: usize) -> Self {
        self.max_filter_nodes = Some(maximum);
        self
    }

    /// Sets the maximum number of relationship linkage members in one request.
    #[must_use]
    pub fn max_relationship_members(mut self, maximum: usize) -> Self {
        self.max_relationship_members = Some(maximum);
        self
    }

    /// Sets the maximum number of operations in an Atomic request.
    #[must_use]
    pub fn max_atomic_operations(mut self, maximum: usize) -> Self {
        self.max_atomic_operations = Some(maximum);
        self
    }

    /// Sets the maximum number of related resources returned by one read.
    ///
    /// SeaORM include loaders can consume this budget while expanding
    /// relationships. Other query adapters are checked after they return.
    #[must_use]
    pub fn max_included_resources(mut self, maximum: usize) -> Self {
        self.max_included_resources = Some(maximum);
        self
    }

    /// Sets the maximum number of related-resource queries in one read.
    ///
    /// Application include loaders consume this budget before each database
    /// query. The request is also checked against returned include counts.
    #[must_use]
    pub fn max_include_queries(mut self, maximum: usize) -> Self {
        self.max_include_queries = Some(maximum);
        self
    }

    /// Validates limits that apply to a planned read.
    ///
    /// # Errors
    ///
    /// Returns a concise description when a configured limit is exceeded.
    pub fn validate_read(&self, plan: &ReadPlan) -> Result<(), String> {
        let (depth, relationships) = include_size(&plan.includes, 1);
        if let Some(maximum) = self.max_include_depth
            && depth > maximum
        {
            return Err(format!(
                "include depth {depth} exceeds the configured maximum of {maximum}"
            ));
        }
        check_maximum(
            "included relationships",
            relationships,
            self.max_include_relationships,
        )?;
        check_maximum(
            "filter expression nodes",
            plan.filter.as_ref().map_or(0, filter_size),
            self.max_filter_nodes,
        )
    }

    /// Validates limits that apply to a base HTTP mutation.
    ///
    /// # Errors
    ///
    /// Returns a concise description when a configured limit is exceeded.
    pub fn validate_mutation(&self, command: &MutationCommand) -> Result<(), String> {
        let members = match command {
            MutationCommand::Create { changeset } | MutationCommand::Update { changeset, .. } => {
                changeset
                    .relationships
                    .values()
                    .map(relationship_member_count)
                    .sum()
            }
            MutationCommand::ModifyRelationship { mutation, .. } => match mutation {
                crate::http::RelationshipMutation::Replace(data) => relationship_member_count(data),
                crate::http::RelationshipMutation::Add(identifiers)
                | crate::http::RelationshipMutation::Remove(identifiers) => identifiers.len(),
            },
            MutationCommand::Delete { .. } | MutationCommand::ReadRelationship { .. } => 0,
        };
        check_maximum(
            "relationship linkage members",
            members,
            self.max_relationship_members,
        )
    }

    /// Validates operation count and relationship linkage in an Atomic batch.
    ///
    /// # Errors
    ///
    /// Returns a concise description when a configured limit is exceeded.
    pub fn validate_atomic(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        check_maximum(
            "Atomic operations",
            operations.len(),
            self.max_atomic_operations,
        )?;
        let members = operations
            .iter()
            .map(|planned| match &planned.operation {
                PlannedOperation::AddResource { changeset, .. }
                | PlannedOperation::UpdateResource { changeset, .. } => changeset
                    .relationships
                    .as_ref()
                    .into_iter()
                    .flat_map(|relationships| relationships.values())
                    .filter_map(|change| change.data.as_ref())
                    .map(relationship_member_count)
                    .sum(),
                PlannedOperation::AddRelationshipMembers { data, .. }
                | PlannedOperation::RemoveRelationshipMembers { data, .. } => data.len(),
                PlannedOperation::UpdateRelationship { data, .. } => {
                    relationship_member_count(data)
                }
                PlannedOperation::RemoveResource { .. } => 0,
            })
            .sum();
        check_maximum(
            "relationship linkage members",
            members,
            self.max_relationship_members,
        )
    }

    /// Wraps an Atomic guard so the standard limits run alongside application limits.
    #[must_use]
    pub fn wrap_atomic_guard(
        &self,
        inner: Arc<dyn AtomicOperationsGuard>,
    ) -> Arc<dyn AtomicOperationsGuard> {
        Arc::new(LimitedAtomicGuard {
            inner,
            limits: self.clone(),
        })
    }

    pub(crate) fn wrap_query_adapter(
        &self,
        inner: Arc<dyn QueryResourceAdapter>,
    ) -> Arc<dyn QueryResourceAdapter> {
        Arc::new(LimitedQueryAdapter {
            inner,
            limits: self.clone(),
        })
    }

    pub(crate) fn seaorm_runtime_budget(&self) -> crate::seaorm::SeaOrmRuntimeBudget {
        crate::seaorm::SeaOrmRuntimeBudget::new(
            self.max_included_resources,
            self.max_include_queries,
        )
    }

    fn validate_included_resource_count(&self, actual: usize) -> Result<(), String> {
        check_maximum("included resources", actual, self.max_included_resources)
    }
}

fn check_maximum(name: &str, actual: usize, maximum: Option<usize>) -> Result<(), String> {
    if let Some(maximum) = maximum
        && actual > maximum
    {
        return Err(format!(
            "{name} count {actual} exceeds the configured maximum of {maximum}"
        ));
    }
    Ok(())
}

fn include_size(nodes: &[IncludeNode], depth: usize) -> (usize, usize) {
    nodes.iter().fold((0, 0), |(maximum_depth, count), node| {
        let (child_depth, child_count) = include_size(&node.children, depth + 1);
        (
            maximum_depth.max(depth).max(child_depth),
            count + 1 + child_count,
        )
    })
}

fn filter_size(filter: &FilterExpression) -> usize {
    match filter {
        FilterExpression::Equals { .. } => 1,
        FilterExpression::Not(child) => 1 + filter_size(child),
        FilterExpression::And(children) | FilterExpression::Or(children) => {
            1 + children.iter().map(filter_size).sum::<usize>()
        }
    }
}

fn relationship_member_count(data: &RelationshipData) -> usize {
    match data {
        RelationshipData::Null => 0,
        RelationshipData::One(_) => 1,
        RelationshipData::Many(identifiers) => identifiers.len(),
    }
}

struct LimitedAtomicGuard {
    inner: Arc<dyn AtomicOperationsGuard>,
    limits: ExecutionLimits,
}

#[async_trait]
impl AtomicOperationsGuard for LimitedAtomicGuard {
    async fn authorize(&self, headers: &HeaderMap, operations: &[PlannedAtomicOperation]) -> bool {
        self.inner.authorize(headers, operations).await
    }

    fn validate_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        self.limits.validate_atomic(operations)?;
        self.inner.validate_limits(operations)
    }
}

struct LimitedQueryAdapter {
    inner: Arc<dyn QueryResourceAdapter>,
    limits: ExecutionLimits,
}

#[async_trait]
impl QueryResourceAdapter for LimitedQueryAdapter {
    async fn collection(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.limits
            .validate_read(plan)
            .map_err(|_| QueryAdapterError::LimitExceeded)?;
        let result = self
            .inner
            .collection_with_limits(resource, plan, &self.limits)
            .await?;
        self.limits
            .validate_included_resource_count(result.included.len())
            .map_err(|_| QueryAdapterError::LimitExceeded)?;
        Ok(result)
    }

    async fn resource(
        &self,
        resource: &ResourceDefinition,
        id: &str,
        plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.limits
            .validate_read(plan)
            .map_err(|_| QueryAdapterError::LimitExceeded)?;
        let result = self
            .inner
            .resource_with_limits(resource, id, plan, &self.limits)
            .await?;
        if let Some(result) = &result {
            self.limits
                .validate_included_resource_count(result.included.len())
                .map_err(|_| QueryAdapterError::LimitExceeded)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionLimits;
    use crate::atomic::{PlannedAtomicOperation, PlannedOperation};
    use crate::document::{RelationshipData, ResourceIdentifier};
    use crate::http::{MutationCommand, RelationshipMutation, ResourceMutationChangeset};
    use crate::query::{FilterExpression, IncludeNode, Page, ReadPlan, SortField};
    use std::collections::BTreeMap;

    fn empty_plan() -> ReadPlan {
        ReadPlan {
            resource_type: "ports".to_owned(),
            filter: None,
            sort: Vec::<SortField>::new(),
            page: Page {
                number: 1,
                size: 10,
                offset: 0,
                limit: 10,
            },
            fieldsets: BTreeMap::new(),
            includes: Vec::new(),
        }
    }

    fn include(name: &str, children: Vec<IncludeNode>) -> IncludeNode {
        IncludeNode {
            public_name: name.to_owned(),
            model_field: name.to_owned(),
            target_type: "ports".to_owned(),
            children,
        }
    }

    #[test]
    fn checks_include_depth_and_filter_complexity() {
        let mut plan = empty_plan();
        plan.includes = vec![include("owner", vec![include("ports", Vec::new())])];
        plan.filter = Some(FilterExpression::And(vec![
            FilterExpression::Equals {
                model_field: "name".to_owned(),
                value: crate::query::FilterValue::String("Ada".to_owned()),
            },
            FilterExpression::Not(Box::new(FilterExpression::Equals {
                model_field: "depth".to_owned(),
                value: crate::query::FilterValue::Null,
            })),
        ]));
        let limits = ExecutionLimits::new()
            .max_include_depth(1)
            .max_filter_nodes(4);
        assert!(limits.validate_read(&plan).is_err());
        plan.includes.pop();
        assert!(limits.validate_read(&plan).is_ok());
    }

    #[test]
    fn checks_relationship_members_in_base_and_atomic_mutations() {
        let identifiers = vec![
            ResourceIdentifier {
                type_name: "tags".to_owned(),
                id: Some("1".to_owned()),
                ..ResourceIdentifier::default()
            },
            ResourceIdentifier {
                type_name: "tags".to_owned(),
                id: Some("2".to_owned()),
                ..ResourceIdentifier::default()
            },
        ];
        let command = MutationCommand::ModifyRelationship {
            id: "1".to_owned(),
            relationship: crate::registry::RelationshipMapping::new("tags", "tags", "tags")
                .to_many(),
            mutation: RelationshipMutation::Add(identifiers.clone()),
        };
        let limits = ExecutionLimits::new().max_relationship_members(1);
        assert!(limits.validate_mutation(&command).is_err());

        let operations = vec![PlannedAtomicOperation {
            operation: PlannedOperation::AddRelationshipMembers {
                reference: crate::atomic::AtomicResourceReference {
                    type_name: "ports".to_owned(),
                    id: Some("1".to_owned()),
                    lid: None,
                    relationship: Some("tags".to_owned()),
                },
                model_field: "tags".to_owned(),
                data: identifiers,
            },
            meta: None,
        }];
        assert!(limits.validate_atomic(&operations).is_err());
        assert!(
            ExecutionLimits::new()
                .max_atomic_operations(0)
                .validate_atomic(&operations)
                .is_err()
        );
        assert_eq!(relationship_members(&RelationshipData::Null), 0);
    }

    fn relationship_members(data: &RelationshipData) -> usize {
        super::relationship_member_count(data)
    }

    #[test]
    fn counts_nested_relationship_changesets() {
        let command = MutationCommand::Create {
            changeset: ResourceMutationChangeset {
                attributes: BTreeMap::new(),
                relationships: BTreeMap::from([(
                    "tags".to_owned(),
                    RelationshipData::Many(vec![ResourceIdentifier::default()]),
                )]),
            },
        };
        assert!(
            ExecutionLimits::new()
                .max_relationship_members(0)
                .validate_mutation(&command)
                .is_err()
        );
    }
}
