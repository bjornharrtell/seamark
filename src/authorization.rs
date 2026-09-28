//! Authorization policies shared by resource reads, ordinary mutations, and
//! Atomic Operations.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;

use crate::atomic::{AtomicOperationsGuard, PlannedAtomicOperation};
use crate::http::{MutationAction, MutationCommand, RequestAuthorizer};
use crate::query::ReadPlan;
use crate::registry::ResourceDefinition;

/// Application policy for every request entry point.
///
/// Use [`SharedAuthorization`] to expose one implementation both as the HTTP
/// request authorizer and as the Atomic Operations guard. The same headers,
/// registry decisions, and application policy can then be applied across
/// reads, includes, ordinary mutations, and Atomic Operations.
#[async_trait]
pub trait AuthorizationPolicy: Send + Sync + 'static {
    /// Authorizes a read when no complete query plan is available.
    async fn authorize_read(
        &self,
        resource_type: &str,
        resource_id: Option<&str>,
        headers: &HeaderMap,
    ) -> bool;

    /// Authorizes a collection or single-resource query, including its
    /// requested fields, filters, sorting, and include tree.
    async fn authorize_query(
        &self,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        _plan: &ReadPlan,
        headers: &HeaderMap,
    ) -> bool {
        self.authorize_read(resource.type_name(), resource_id, headers)
            .await
    }

    /// Authorizes an ordinary resource or relationship mutation.
    ///
    /// The default denies writes so read permission cannot grant write access.
    async fn authorize_mutation(
        &self,
        _action: MutationAction,
        _resource: &ResourceDefinition,
        _resource_id: Option<&str>,
        _command: &MutationCommand,
        _headers: &HeaderMap,
    ) -> bool {
        false
    }

    /// Authorizes every planned operation in an Atomic Operations request.
    async fn authorize_atomic(
        &self,
        _headers: &HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        false
    }

    /// Applies application-specific limits to an ordinary mutation.
    fn validate_mutation_limits(
        &self,
        _action: MutationAction,
        _resource: &ResourceDefinition,
        _command: &MutationCommand,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Applies application-specific limits to an Atomic Operations request.
    fn validate_atomic_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        Ok(())
    }
}

/// Adapts an application policy to both Seamark authorization interfaces.
///
/// Keep one `Arc<SharedAuthorization<_>>` and pass clones to
/// `ApiBuilder::new` and `ApiBuilder::atomic_operations` to share policy
/// behavior across enabled endpoints.
pub struct SharedAuthorization<P> {
    policy: P,
}

impl<P> SharedAuthorization<P> {
    /// Wraps one application authorization policy.
    #[must_use]
    pub fn new(policy: P) -> Self {
        Self { policy }
    }

    /// Returns the wrapped policy.
    #[must_use]
    pub fn policy(&self) -> &P {
        &self.policy
    }
}

#[async_trait]
impl<P> RequestAuthorizer for SharedAuthorization<P>
where
    P: AuthorizationPolicy,
{
    async fn authorize(
        &self,
        resource_type: &str,
        resource_id: Option<&str>,
        headers: &HeaderMap,
    ) -> bool {
        self.policy
            .authorize_read(resource_type, resource_id, headers)
            .await
    }

    async fn authorize_query(
        &self,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        plan: &ReadPlan,
        headers: &HeaderMap,
    ) -> bool {
        self.policy
            .authorize_query(resource, resource_id, plan, headers)
            .await
    }

    async fn authorize_mutation(
        &self,
        action: MutationAction,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        command: &MutationCommand,
        headers: &HeaderMap,
    ) -> bool {
        self.policy
            .authorize_mutation(action, resource, resource_id, command, headers)
            .await
    }

    fn validate_mutation_limits(
        &self,
        action: MutationAction,
        resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<(), String> {
        self.policy
            .validate_mutation_limits(action, resource, command)
    }
}

#[async_trait]
impl<P> AtomicOperationsGuard for SharedAuthorization<P>
where
    P: AuthorizationPolicy,
{
    async fn authorize(&self, headers: &HeaderMap, operations: &[PlannedAtomicOperation]) -> bool {
        self.policy.authorize_atomic(headers, operations).await
    }

    fn validate_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        self.policy.validate_atomic_limits(operations)
    }
}

/// Combines authorization policies with an all-must-allow rule.
pub struct AllOfAuthorizationPolicy {
    policies: Vec<Arc<dyn AuthorizationPolicy>>,
}

impl AllOfAuthorizationPolicy {
    /// Combines the supplied policies. An empty list denies every request.
    #[must_use]
    pub fn new(policies: Vec<Arc<dyn AuthorizationPolicy>>) -> Self {
        Self { policies }
    }
}

#[async_trait]
impl AuthorizationPolicy for AllOfAuthorizationPolicy {
    async fn authorize_read(
        &self,
        resource_type: &str,
        resource_id: Option<&str>,
        headers: &HeaderMap,
    ) -> bool {
        !self.policies.is_empty()
            && allow_all(
                self.policies
                    .iter()
                    .map(|policy| policy.authorize_read(resource_type, resource_id, headers)),
            )
            .await
    }

    async fn authorize_query(
        &self,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        plan: &ReadPlan,
        headers: &HeaderMap,
    ) -> bool {
        !self.policies.is_empty()
            && allow_all(
                self.policies
                    .iter()
                    .map(|policy| policy.authorize_query(resource, resource_id, plan, headers)),
            )
            .await
    }

    async fn authorize_mutation(
        &self,
        action: MutationAction,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        command: &MutationCommand,
        headers: &HeaderMap,
    ) -> bool {
        !self.policies.is_empty()
            && allow_all(self.policies.iter().map(|policy| {
                policy.authorize_mutation(action, resource, resource_id, command, headers)
            }))
            .await
    }

    async fn authorize_atomic(
        &self,
        headers: &HeaderMap,
        operations: &[PlannedAtomicOperation],
    ) -> bool {
        !self.policies.is_empty()
            && allow_all(
                self.policies
                    .iter()
                    .map(|policy| policy.authorize_atomic(headers, operations)),
            )
            .await
    }

    fn validate_mutation_limits(
        &self,
        action: MutationAction,
        resource: &ResourceDefinition,
        command: &MutationCommand,
    ) -> Result<(), String> {
        for policy in &self.policies {
            policy.validate_mutation_limits(action, resource, command)?;
        }
        Ok(())
    }

    fn validate_atomic_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        for policy in &self.policies {
            policy.validate_atomic_limits(operations)?;
        }
        Ok(())
    }
}

async fn allow_all<'a>(
    futures: impl Iterator<Item = impl std::future::Future<Output = bool> + 'a>,
) -> bool {
    for future in futures {
        if !future.await {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::http::HeaderMap;

    use super::{AllOfAuthorizationPolicy, AuthorizationPolicy, SharedAuthorization};
    use crate::atomic::AtomicOperationsGuard;
    use crate::http::{MutationAction, MutationCommand, RequestAuthorizer};
    use crate::registry::ResourceDefinition;

    struct TestPolicy {
        allow: bool,
    }

    #[async_trait]
    impl AuthorizationPolicy for TestPolicy {
        async fn authorize_read(
            &self,
            _resource_type: &str,
            _resource_id: Option<&str>,
            _headers: &HeaderMap,
        ) -> bool {
            self.allow
        }

        async fn authorize_mutation(
            &self,
            _action: MutationAction,
            _resource: &ResourceDefinition,
            _resource_id: Option<&str>,
            _command: &MutationCommand,
            _headers: &HeaderMap,
        ) -> bool {
            self.allow
        }

        async fn authorize_atomic(
            &self,
            _headers: &HeaderMap,
            _operations: &[crate::atomic::PlannedAtomicOperation],
        ) -> bool {
            self.allow
        }
    }

    #[tokio::test]
    async fn shared_policy_implements_http_and_atomic_authorization() {
        let shared = Arc::new(SharedAuthorization::new(TestPolicy { allow: true }));
        let http: Arc<dyn RequestAuthorizer> = shared.clone();
        let atomic: Arc<dyn AtomicOperationsGuard> = shared;
        let resource = ResourceDefinition::new("people", "person_id");
        let headers = HeaderMap::new();
        let command = MutationCommand::Delete { id: "1".to_owned() };

        assert!(http.authorize("people", None, &headers).await);
        assert!(
            http.authorize_mutation(
                MutationAction::Delete,
                &resource,
                Some("1"),
                &command,
                &headers,
            )
            .await
        );
        assert!(atomic.authorize(&headers, &[]).await);
    }

    #[tokio::test]
    async fn all_of_policy_denies_when_any_policy_denies() {
        let policy = AllOfAuthorizationPolicy::new(vec![
            Arc::new(TestPolicy { allow: true }),
            Arc::new(TestPolicy { allow: false }),
        ]);

        assert!(
            !policy
                .authorize_read("people", None, &HeaderMap::new())
                .await
        );
    }

    #[tokio::test]
    async fn all_of_policy_denies_when_empty() {
        let policy = AllOfAuthorizationPolicy::new(Vec::new());

        assert!(
            !policy
                .authorize_read("people", None, &HeaderMap::new())
                .await
        );
    }
}
