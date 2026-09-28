//! Axum HTTP integration for the initial read-only vertical slice.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::header::{CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::document::{
    ErrorObject, ErrorSource, JsonApiDocument, PrimaryData, Relationship, RelationshipData,
    ResourceObject,
};
use crate::json::parse_unique_members;
use crate::media;
pub use crate::projection::{ProjectionError, project_resource};
use crate::projection::{include_relationships_by_type, project_resource_with_includes};
use crate::query::{
    IncludeNode, PaginationConfig, PlannedField, ReadPlan, ReadPlanError, ReadQuery, plan_read,
    plan_resource_read,
};
use crate::registry::{
    AttributePermission, RelationshipCardinality, RelationshipMapping, RelationshipPermission,
    ResourceDefinition, ResourcePermission, ResourceRegistry,
};

const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";

/// A resource record returned by a persistence adapter.
///
/// Attribute and relationship map keys are internal field names. The HTTP
/// layer projects only fields declared in the public resource registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdapterResource {
    /// A normalized persistent identifier for the public JSON:API resource.
    ///
    /// The adapter resolves the registered
    /// [`ResourceDefinition::identifier_field`] and returns its value in this
    /// representation; the HTTP layer does not interpret raw model fields.
    pub id: String,
    /// Attribute values keyed by internal field name.
    pub attributes: BTreeMap<String, Value>,
    /// Relationship objects keyed by internal field name.
    pub relationships: BTreeMap<String, Relationship>,
}

/// An adapter read failed.
///
/// Adapter-specific failure details are intentionally not exposed in HTTP
/// error documents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterError;

/// A query adapter failure with an HTTP-relevant category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryAdapterError {
    /// The query could not be completed because of an internal failure.
    ReadFailed,
    /// The caller was denied access to the planned query.
    NotAuthorized,
    /// The planned query exceeded an application-configured execution limit.
    LimitExceeded,
    /// The adapter does not implement single-resource query execution.
    ResourceReadUnsupported,
}

/// A validated base JSON:API mutation presented to an application adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum MutationCommand {
    /// Create one resource in the addressed collection.
    Create {
        /// Public fields mapped to internal persistence names.
        changeset: ResourceMutationChangeset,
    },
    /// Update the supplied fields on one existing resource.
    Update {
        /// Persistent resource identifier from the request URL.
        id: String,
        /// Only fields present in the request are included.
        changeset: ResourceMutationChangeset,
    },
    /// Delete one existing resource.
    Delete {
        /// Persistent resource identifier from the request URL.
        id: String,
    },
    /// Read the linkage of one relationship.
    ReadRelationship {
        /// Persistent identifier of the relationship owner.
        id: String,
        /// Registered public relationship mapping.
        relationship: RelationshipMapping,
    },
    /// Modify the linkage of one relationship.
    ModifyRelationship {
        /// Persistent identifier of the relationship owner.
        id: String,
        /// Registered public relationship mapping.
        relationship: RelationshipMapping,
        /// Linkage mutation selected by the HTTP method.
        mutation: RelationshipMutation,
    },
}

/// Public fields mapped to a registered resource's internal model fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceMutationChangeset {
    /// Present attributes keyed by internal field name.
    pub attributes: BTreeMap<String, Value>,
    /// Present relationship linkage keyed by internal field name.
    pub relationships: BTreeMap<String, RelationshipData>,
}

/// A to-many relationship linkage change.
#[derive(Clone, Debug, PartialEq)]
pub enum RelationshipMutation {
    /// Replace the complete to-one or to-many linkage.
    Replace(RelationshipData),
    /// Idempotently add the supplied to-many linkage members.
    Add(Vec<crate::document::ResourceIdentifier>),
    /// Idempotently remove the supplied to-many linkage members.
    Remove(Vec<crate::document::ResourceIdentifier>),
}

/// The result of a base JSON:API mutation adapter operation.
#[derive(Clone, Debug, PartialEq)]
pub enum MutationOutcome {
    /// The updated or created resource representation.
    Resource(AdapterResource),
    /// The current relationship linkage after the operation.
    Relationship(RelationshipData),
    /// A successful resource deletion.
    Deleted,
}

/// An application mutation adapter failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationAdapterError {
    /// The addressed resource does not exist.
    NotFound,
    /// A related resource does not exist.
    RelatedResourceNotFound,
    /// The request conflicts with current application state.
    Conflict,
    /// The application does not permit the requested operation.
    Unsupported,
    /// The operation failed for an internal reason.
    Failed,
}

/// Executes validated base JSON:API resource and relationship operations.
///
/// The adapter owns persistence and transaction boundaries. It must make each
/// command atomic and return the resulting representation or linkage. For
/// to-many `Add` and `Remove`, already-present additions and already-absent
/// removals must succeed without creating duplicate linkage.
#[async_trait]
pub trait MutationResourceAdapter: Send + Sync + 'static {
    /// Executes one validated mutation for a registered resource.
    async fn execute(
        &self,
        resource: &ResourceDefinition,
        command: MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError>;

    /// Validates this adapter against the complete registry before routes are
    /// served. Custom adapters may use the default when they dispatch
    /// dynamically.
    fn validate_registry(&self, _registry: &ResourceRegistry) -> Result<(), String> {
        Ok(())
    }
}

/// The category of a base HTTP action presented to authorization policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationAction {
    /// Create a resource.
    Create,
    /// Update a resource.
    Update,
    /// Delete a resource.
    Delete,
    /// Read relationship linkage.
    ReadRelationship,
    /// Replace relationship linkage.
    ReplaceRelationship,
    /// Add relationship linkage members.
    AddRelationshipMembers,
    /// Remove relationship linkage members.
    RemoveRelationshipMembers,
}

/// A resource from the `included` member of a planned collection read.
#[derive(Clone, Debug, PartialEq)]
pub struct AdapterIncludedResource {
    /// The public JSON:API type of the included resource.
    pub resource_type: String,
    /// The mapped persistence record.
    pub resource: AdapterResource,
}

/// Results returned by an adapter for a planned collection read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryCollectionResult {
    /// Root collection resources.
    pub resources: Vec<AdapterResource>,
    /// Compound resources requested by the validated include plan.
    pub included: Vec<AdapterIncludedResource>,
}

/// Results returned by an adapter for a planned single-resource read.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResourceResult {
    /// The root resource.
    pub resource: AdapterResource,
    /// Compound resources requested by the validated include plan.
    pub included: Vec<AdapterIncludedResource>,
}

/// Results returned by an adapter for a related-resource read.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryRelatedResult {
    /// A to-one related resource, or `None` for an empty relationship.
    One(Option<QueryResourceResult>),
    /// A to-many related collection.
    Many(QueryCollectionResult),
}

/// The asynchronous read operations required by the initial GET routes.
#[async_trait]
pub trait ResourceAdapter: Send + Sync + 'static {
    /// Reads a resource collection.
    async fn collection(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<Vec<AdapterResource>, AdapterError>;

    /// Reads one resource by persistent identifier.
    async fn resource(
        &self,
        resource: &ResourceDefinition,
        id: &str,
    ) -> Result<Option<AdapterResource>, AdapterError>;
}

/// Executes validated collection and single-resource plans produced by
/// [`ApiBuilder::queries`].
///
/// Authorization and execution-limit failures should use their corresponding
/// [`QueryAdapterError`] variants so the router can return client-appropriate
/// JSON:API error responses. Other failures are treated as internal read
/// errors.
#[async_trait]
pub trait QueryResourceAdapter: Send + Sync + 'static {
    /// Executes the plan for one registered resource.
    async fn collection(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError>;

    /// Executes a collection plan with the router's shared execution limits.
    ///
    /// Adapters that expand includes during execution can override this
    /// method to consume runtime budgets. The default keeps custom adapters
    /// working and the router checks their returned include count afterward.
    async fn collection_with_limits(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
        _limits: &crate::limits::ExecutionLimits,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.collection(resource, plan).await
    }

    /// Validates this adapter against the complete registry before routes are
    /// served. Custom adapters may use the default when they dispatch
    /// dynamically.
    fn validate_registry(&self, _registry: &ResourceRegistry) -> Result<(), String> {
        Ok(())
    }

    /// Executes the plan for one resource addressed by its persistent ID.
    ///
    /// The default reports the unsupported operation explicitly so existing
    /// adapters remain source-compatible while opting into single-resource
    /// query execution requires an explicit implementation.
    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
        _plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        Err(QueryAdapterError::ResourceReadUnsupported)
    }

    /// Executes a resource plan with the router's shared execution limits.
    ///
    /// Adapters that expand includes during execution can override this
    /// method to consume runtime budgets. The default keeps custom adapters
    /// working and the router checks their returned include count afterward.
    async fn resource_with_limits(
        &self,
        resource: &ResourceDefinition,
        id: &str,
        plan: &ReadPlan,
        _limits: &crate::limits::ExecutionLimits,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.resource(resource, id, plan).await
    }

    /// Executes a related-resource read for one relationship.
    ///
    /// The default reports the unsupported operation explicitly so adapters
    /// keep working while opting into related-resource routes.
    async fn related(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
        _relationship: &RelationshipMapping,
        _plan: &ReadPlan,
    ) -> Result<QueryRelatedResult, QueryAdapterError> {
        Err(QueryAdapterError::ResourceReadUnsupported)
    }
}

/// Authorizes access before a persistence adapter is called.
#[async_trait]
pub trait RequestAuthorizer: Send + Sync + 'static {
    /// Returns whether the request may read the resource or collection.
    async fn authorize(
        &self,
        resource_type: &str,
        resource_id: Option<&str>,
        headers: &HeaderMap,
    ) -> bool;

    /// Authorizes a complete validated query plan, including filters, sort
    /// fields, fieldsets, and requested relationships. Implementations that
    /// distinguish access to includes or query capabilities should override
    /// this method.
    async fn authorize_query(
        &self,
        resource: &ResourceDefinition,
        resource_id: Option<&str>,
        _plan: &ReadPlan,
        headers: &HeaderMap,
    ) -> bool {
        self.authorize(resource.type_name(), resource_id, headers)
            .await
    }

    /// Authorizes a base mutation or relationship action.
    ///
    /// The default denies the mutation. Applications must explicitly provide
    /// a mutation policy; successful read authorization grants no write access.
    async fn authorize_mutation(
        &self,
        _action: MutationAction,
        _resource: &ResourceDefinition,
        _resource_id: Option<&str>,
        _command: &MutationCommand,
        headers: &HeaderMap,
    ) -> bool {
        let _ = headers;
        false
    }

    /// Checks request-specific mutation limits before adapter execution.
    ///
    /// # Errors
    ///
    /// Returns a concise explanation if a configured limit is exceeded.
    fn validate_mutation_limits(
        &self,
        _action: MutationAction,
        _resource: &ResourceDefinition,
        _command: &MutationCommand,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// An authorizer that permits all reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAuthorizer;

#[async_trait]
impl RequestAuthorizer for AllowAllAuthorizer {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }

    async fn authorize_mutation(
        &self,
        _action: MutationAction,
        _resource: &ResourceDefinition,
        _resource_id: Option<&str>,
        _command: &MutationCommand,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }
}

struct ApiState {
    registry: Arc<ResourceRegistry>,
    adapter: Option<Arc<dyn ResourceAdapter>>,
    authorizer: Arc<dyn RequestAuthorizer>,
    query_adapter: Option<Arc<dyn QueryResourceAdapter>>,
    mutation_adapter: Option<Arc<dyn MutationResourceAdapter>>,
    pagination: Option<PaginationConfig>,
    execution_limits: Option<crate::limits::ExecutionLimits>,
    links: bool,
}

/// A composable router builder with separate opt-ins for simple reads,
/// queries, ordinary mutations, and Atomic Operations.
///
/// Every capability is disabled by default. Query-only consumers can provide
/// a [`QueryResourceAdapter`] without writing a forwarding
/// [`ResourceAdapter`] implementation.
pub struct ApiBuilder {
    registry: Arc<ResourceRegistry>,
    authorizer: Arc<dyn RequestAuthorizer>,
    adapter: Option<Arc<dyn ResourceAdapter>>,
    query_adapter: Option<Arc<dyn QueryResourceAdapter>>,
    mutation_adapter: Option<Arc<dyn MutationResourceAdapter>>,
    pagination: Option<PaginationConfig>,
    execution_limits: Option<crate::limits::ExecutionLimits>,
    atomic: Option<AtomicApiConfig>,
    jsonapi_fallback: bool,
    links: bool,
}

struct AtomicApiConfig {
    database: DatabaseConnection,
    guard: Arc<dyn crate::atomic::AtomicOperationsGuard>,
    handler: Arc<dyn crate::atomic::AtomicOperationHandler>,
    href_resolver: Option<Arc<dyn crate::atomic::AtomicHrefResolver>>,
}

impl ApiBuilder {
    /// Creates a read-disabled builder. Add a read adapter or query support
    /// explicitly before building the router.
    #[must_use]
    pub fn new(registry: Arc<ResourceRegistry>, authorizer: Arc<dyn RequestAuthorizer>) -> Self {
        Self {
            registry,
            authorizer,
            adapter: None,
            query_adapter: None,
            mutation_adapter: None,
            pagination: None,
            execution_limits: None,
            atomic: None,
            jsonapi_fallback: false,
            links: false,
        }
    }

    /// Enables simple collection/resource reads without query planning.
    #[must_use]
    pub fn reads(mut self, adapter: Arc<dyn ResourceAdapter>) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// Enables validated collection queries and single-resource query plans.
    #[must_use]
    pub fn queries(
        mut self,
        adapter: Arc<dyn QueryResourceAdapter>,
        pagination: PaginationConfig,
    ) -> Self {
        self.query_adapter = Some(adapter);
        self.pagination = Some(pagination);
        self
    }

    /// Enables ordinary JSON:API resource and relationship mutations.
    #[must_use]
    pub fn mutations(mut self, adapter: Arc<dyn MutationResourceAdapter>) -> Self {
        self.mutation_adapter = Some(adapter);
        self
    }

    /// Applies reusable include, filter, relationship, and Atomic batch limits.
    #[must_use]
    pub fn limits(mut self, limits: crate::limits::ExecutionLimits) -> Self {
        self.execution_limits = Some(limits);
        self
    }

    /// Installs a JSON:API `404`/`405` fallback on the component router.
    ///
    /// The fallback responds to unmatched paths beneath the component router
    /// with structured JSON:API errors. It is opt-in so a Seamark component
    /// router does not capture unmatched paths belonging to the rest of an
    /// application; install it only on the router that owns the JSON:API
    /// surface, or install [`not_found_fallback`] as the application fallback.
    #[must_use]
    pub fn jsonapi_fallback(mut self) -> Self {
        self.jsonapi_fallback = true;
        self
    }

    /// Emits JSON:API `self` and pagination links in read responses.
    ///
    /// Links are opt-in so a default response stays minimal. When enabled,
    /// collection and single-resource responses carry a document `self` link,
    /// resource objects carry a `self` link, and paginated collections carry
    /// `first`/`prev`/`next` and a `last` placeholder. The corresponding GET
    /// routes must be served by the same router.
    #[must_use]
    pub fn links(mut self) -> Self {
        self.links = true;
        self
    }

    /// Enables the Atomic Operations endpoint as an independent capability.
    #[must_use]
    pub fn atomic_operations(
        mut self,
        database: DatabaseConnection,
        guard: Arc<dyn crate::atomic::AtomicOperationsGuard>,
        handler: Arc<dyn crate::atomic::AtomicOperationHandler>,
    ) -> Self {
        self.atomic = Some(AtomicApiConfig {
            database,
            guard,
            handler,
            href_resolver: None,
        });
        self
    }

    /// Enables Atomic Operations with an application `href` resolver.
    #[must_use]
    pub fn atomic_operations_with_href_resolver(
        mut self,
        database: DatabaseConnection,
        guard: Arc<dyn crate::atomic::AtomicOperationsGuard>,
        handler: Arc<dyn crate::atomic::AtomicOperationHandler>,
        href_resolver: Arc<dyn crate::atomic::AtomicHrefResolver>,
    ) -> Self {
        self.atomic = Some(AtomicApiConfig {
            database,
            guard,
            handler,
            href_resolver: Some(href_resolver),
        });
        self
    }

    /// Builds the configured component router.
    pub fn build(self) -> Router {
        self.try_build()
            .unwrap_or_else(|error| panic!("invalid Seamark API configuration: {error}"))
    }

    /// Validates standard adapters and builds the configured component router.
    ///
    /// # Errors
    ///
    /// Returns an actionable error when an adapter reports missing or
    /// conflicting registrations for the resource registry.
    pub fn try_build(self) -> Result<Router, ApiConfigurationError> {
        if let Some(adapter) = &self.query_adapter {
            adapter
                .validate_registry(&self.registry)
                .map_err(ApiConfigurationError::QueryAdapter)?;
        }
        if let Some(adapter) = &self.mutation_adapter {
            adapter
                .validate_registry(&self.registry)
                .map_err(ApiConfigurationError::MutationAdapter)?;
        }
        if let Some(atomic) = &self.atomic {
            atomic
                .handler
                .validate_registry(&self.registry)
                .map_err(ApiConfigurationError::AtomicHandler)?;
        }
        Ok(self.build_unchecked())
    }

    fn build_unchecked(self) -> Router {
        let limits = self.execution_limits.clone();
        let atomic_registry = self.registry.clone();
        let query_adapter = match (self.query_adapter, limits.as_ref()) {
            (Some(adapter), Some(limits)) => Some(limits.wrap_query_adapter(adapter)),
            (adapter, _) => adapter,
        };
        let mut router = build_router(ApiState {
            registry: self.registry,
            adapter: self.adapter,
            authorizer: self.authorizer,
            query_adapter,
            mutation_adapter: self.mutation_adapter,
            pagination: self.pagination,
            execution_limits: limits.clone(),
            links: self.links,
        });
        if let Some(atomic) = self.atomic {
            let guard = limits.as_ref().map_or(atomic.guard.clone(), |limits| {
                limits.wrap_atomic_guard(atomic.guard.clone())
            });
            let atomic_router = if let Some(href_resolver) = atomic.href_resolver {
                crate::atomic_http::router_with_href_resolver(
                    atomic_registry,
                    atomic.database,
                    guard,
                    atomic.handler,
                    href_resolver,
                )
            } else {
                crate::atomic_http::router(atomic_registry, atomic.database, guard, atomic.handler)
            };
            router = router.merge(atomic_router);
        }
        if self.jsonapi_fallback {
            router = router.fallback(not_found_fallback);
        }
        router
    }
}

/// An invalid adapter registration detected while assembling an API router.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiConfigurationError {
    /// The configured query adapter is inconsistent with the resource registry.
    QueryAdapter(String),
    /// The configured base mutation adapter is inconsistent with the registry.
    MutationAdapter(String),
    /// The configured Atomic Operations handler is inconsistent with the registry.
    AtomicHandler(String),
}

impl std::fmt::Display for ApiConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueryAdapter(message) => write!(formatter, "invalid query adapter: {message}"),
            Self::MutationAdapter(message) => {
                write!(formatter, "invalid mutation adapter: {message}")
            }
            Self::AtomicHandler(message) => {
                write!(formatter, "invalid Atomic Operations handler: {message}")
            }
        }
    }
}

impl std::error::Error for ApiConfigurationError {}

fn build_router(state: ApiState) -> Router {
    let mutation_routes = state.mutation_adapter.is_some();
    let read_routes = state.adapter.is_some() || state.query_adapter.is_some();
    let mut router = Router::new();
    if read_routes && mutation_routes {
        router = router
            .route(
                "/{resource_type}",
                get(get_collection)
                    .post(create_resource)
                    .fallback(method_not_allowed),
            )
            .route(
                "/{resource_type}/{id}",
                get(get_resource)
                    .patch(update_resource)
                    .delete(delete_resource)
                    .fallback(method_not_allowed),
            );
    } else if read_routes {
        router = router
            .route(
                "/{resource_type}",
                get(get_collection).fallback(method_not_allowed),
            )
            .route(
                "/{resource_type}/{id}",
                get(get_resource).fallback(method_not_allowed),
            );
    } else if mutation_routes {
        router = router
            .route(
                "/{resource_type}",
                axum::routing::post(create_resource).fallback(method_not_allowed),
            )
            .route(
                "/{resource_type}/{id}",
                axum::routing::patch(update_resource)
                    .delete(delete_resource)
                    .fallback(method_not_allowed),
            );
    }
    if mutation_routes {
        router = router.route(
            "/{resource_type}/{id}/relationships/{relationship}",
            get(get_relationship)
                .patch(replace_relationship)
                .post(add_relationship_members)
                .delete(remove_relationship_members)
                .fallback(method_not_allowed),
        );
    }
    if state.query_adapter.is_some() {
        router = router.route(
            "/{resource_type}/{id}/{relationship}",
            get(get_related).fallback(method_not_allowed),
        );
    }
    router.with_state(Arc::new(state))
}

/// Returns a JSON:API 404 response for an unmatched application route.
///
/// Install this as the application's final Axum fallback when unmatched
/// requests should use JSON:API errors:
///
/// ```no_run
/// # use axum::Router;
/// # use seamark::http;
/// let app: Router = Router::new().fallback(http::not_found_fallback);
/// ```
///
/// This handler is opt-in so a Seamark component router does not capture
/// unmatched paths belonging to the rest of an application.
pub async fn not_found_fallback(headers: HeaderMap) -> Response {
    if !accepts_jsonapi(&headers) {
        return request_error_response(RequestValidationError::NotAcceptable);
    }
    protocol_error(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "Route not found",
        Some("The requested URL does not match a registered JSON:API route.".to_owned()),
        None,
    )
}

async fn method_not_allowed(headers: HeaderMap) -> Response {
    if !accepts_jsonapi(&headers) {
        return request_error_response(RequestValidationError::NotAcceptable);
    }
    protocol_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Method not allowed",
        Some("The requested method is not supported for this route.".to_owned()),
        None,
    )
}

async fn get_collection(
    State(state): State<Arc<ApiState>>,
    Path(resource_type): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = validate_request(&headers, None) {
        return response;
    }
    let has_query = query.as_deref().is_some_and(|query| !query.is_empty());
    if has_query && state.query_adapter.is_none() {
        return request_error_response(RequestValidationError::UnsupportedQuery(
            query.as_deref().and_then(first_query_parameter),
        ));
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => {
            return protocol_error(
                StatusCode::NOT_FOUND,
                "unknown_resource_type",
                "Resource type not found",
                Some(format!(
                    "Resource type `{resource_type}` is not registered."
                )),
                None,
            );
        }
    };
    let (plan, include_requested) = if state.query_adapter.is_some() {
        let Some(pagination) = state.pagination.as_ref() else {
            return request_error_response(RequestValidationError::UnsupportedQuery(
                query.as_deref().and_then(first_query_parameter),
            ));
        };
        let query = match parse_read_query(query.as_deref().unwrap_or_default()) {
            Ok(query) => query,
            Err(error) => return query_parse_error(error),
        };
        let include_requested = !query.includes.is_empty();
        match plan_read(&state.registry, &resource_type, &query, pagination) {
            Ok(plan) => {
                if let Some(limits) = &state.execution_limits
                    && let Err(message) = limits.validate_read(&plan)
                {
                    return limit_exceeded_error(message);
                }
                (Some(plan), include_requested)
            }
            Err(error) => return read_plan_error(error),
        }
    } else {
        (None, false)
    };
    let allowed = if let Some(plan) = plan.as_ref() {
        state
            .authorizer
            .authorize_query(definition, None, plan, &headers)
            .await
    } else {
        state
            .authorizer
            .authorize(&resource_type, None, &headers)
            .await
    };
    if !allowed {
        return forbidden_error();
    }

    let (records, included) =
        if let (Some(query_adapter), Some(plan)) = (state.query_adapter.as_ref(), plan.as_ref()) {
            match query_adapter.collection(definition, plan).await {
                Ok(result) => (result.resources, result.included),
                Err(error) => return query_adapter_error(error),
            }
        } else {
            let Some(adapter) = state.adapter.as_ref() else {
                return read_not_configured_error();
            };
            match adapter.collection(definition).await {
                Ok(records) => (records, Vec::new()),
                Err(_) => return adapter_error(),
            }
        };
    let included_relationships = plan
        .as_ref()
        .map(include_relationships_by_type)
        .unwrap_or_default();
    let resources = match records
        .iter()
        .map(|record| {
            project_resource_with_includes(
                definition,
                record,
                included_relationships
                    .get(definition.type_name())
                    .unwrap_or(&std::collections::BTreeSet::new()),
            )
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(resources) => resources,
        Err(_) => return adapter_error(),
    };
    let page = plan.as_ref().map(|plan| plan.page);
    let returned = resources.len() as u64;
    let included = match included
        .iter()
        .map(|included| {
            let definition = state
                .registry
                .resource(&included.resource_type)
                .map_err(|_| ProjectionError::UnknownResourceType)?;
            project_resource_with_includes(
                definition,
                &included.resource,
                included_relationships
                    .get(&included.resource_type)
                    .unwrap_or(&std::collections::BTreeSet::new()),
            )
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(included) => included,
        Err(_) => return adapter_error(),
    };
    let mut document = JsonApiDocument {
        data: Some(PrimaryData::Many(resources)),
        included: (include_requested || !included.is_empty()).then_some(included),
        ..JsonApiDocument::default()
    };
    if document.validate_response().is_err() {
        let sparse_fieldset_exception_applies = document.included.is_some()
            && plan
                .as_ref()
                .is_some_and(has_sparse_fieldset_include_relationship)
            && document
                .validate_response_with_sparse_fieldset_exception()
                .is_ok();
        if !sparse_fieldset_exception_applies {
            return adapter_error();
        }
    }
    if let Some(plan) = plan.as_ref() {
        if let (Some(PrimaryData::Many(resources)), Some(fieldset)) = (
            document.data.as_mut(),
            plan.fieldsets.get(definition.type_name()),
        ) {
            for resource in resources {
                apply_fieldset(resource, fieldset);
            }
        }
        if let Some(included) = document.included.as_mut() {
            for resource in included {
                if let Some(fieldset) = plan.fieldsets.get(&resource.type_name) {
                    apply_fieldset(resource, fieldset);
                }
            }
        }
    }
    if state.links {
        apply_collection_links(
            &mut document,
            &resource_type,
            query.as_deref(),
            page,
            returned,
        );
    }
    respond_with_validated_document(StatusCode::OK, document)
}

async fn get_resource(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = validate_request(&headers, None) {
        return response;
    }
    let has_query = query.as_deref().is_some_and(|query| !query.is_empty());
    if has_query && state.query_adapter.is_none() {
        return request_error_response(RequestValidationError::UnsupportedQuery(
            query.as_deref().and_then(first_query_parameter),
        ));
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => {
            return protocol_error(
                StatusCode::NOT_FOUND,
                "unknown_resource_type",
                "Resource type not found",
                Some(format!(
                    "Resource type `{resource_type}` is not registered."
                )),
                None,
            );
        }
    };
    let (plan, include_requested) = if state.query_adapter.is_some() {
        let Some(pagination) = state.pagination.as_ref() else {
            return request_error_response(RequestValidationError::UnsupportedQuery(
                query.as_deref().and_then(first_query_parameter),
            ));
        };
        let query = match parse_read_query(query.as_deref().unwrap_or_default()) {
            Ok(query) => query,
            Err(error) => return query_parse_error(error),
        };
        let include_requested = !query.includes.is_empty();
        match plan_resource_read(&state.registry, &resource_type, &query, pagination) {
            Ok(plan) => {
                if let Some(limits) = &state.execution_limits
                    && let Err(message) = limits.validate_read(&plan)
                {
                    return limit_exceeded_error(message);
                }
                (Some(plan), include_requested)
            }
            Err(error) => return read_plan_error(error),
        }
    } else {
        (None, false)
    };
    let allowed = if let Some(plan) = plan.as_ref() {
        state
            .authorizer
            .authorize_query(definition, Some(&id), plan, &headers)
            .await
    } else {
        state
            .authorizer
            .authorize(&resource_type, Some(&id), &headers)
            .await
    };
    if !allowed {
        return forbidden_error();
    }

    let (record, included) =
        if let (Some(query_adapter), Some(plan)) = (state.query_adapter.as_ref(), plan.as_ref()) {
            match query_adapter.resource(definition, &id, plan).await {
                Ok(Some(result)) => (result.resource, result.included),
                Ok(None) => {
                    return protocol_error(
                        StatusCode::NOT_FOUND,
                        "resource_not_found",
                        "Resource not found",
                        Some(format!("No `{resource_type}` resource has id `{id}`.")),
                        None,
                    );
                }
                Err(error) => return query_adapter_error(error),
            }
        } else {
            let Some(adapter) = state.adapter.as_ref() else {
                return read_not_configured_error();
            };
            match adapter.resource(definition, &id).await {
                Ok(Some(record)) => (record, Vec::new()),
                Ok(None) => {
                    return protocol_error(
                        StatusCode::NOT_FOUND,
                        "resource_not_found",
                        "Resource not found",
                        Some(format!("No `{resource_type}` resource has id `{id}`.")),
                        None,
                    );
                }
                Err(_) => return adapter_error(),
            }
        };
    let included_relationships = plan
        .as_ref()
        .map(include_relationships_by_type)
        .unwrap_or_default();
    let resource = match project_resource_with_includes(
        definition,
        &record,
        included_relationships
            .get(definition.type_name())
            .unwrap_or(&std::collections::BTreeSet::new()),
    ) {
        Ok(resource) => resource,
        Err(_) => return adapter_error(),
    };
    let included = match included
        .iter()
        .map(|included| {
            let definition = state
                .registry
                .resource(&included.resource_type)
                .map_err(|_| ProjectionError::UnknownResourceType)?;
            project_resource_with_includes(
                definition,
                &included.resource,
                included_relationships
                    .get(&included.resource_type)
                    .unwrap_or(&std::collections::BTreeSet::new()),
            )
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(included) => included,
        Err(_) => return adapter_error(),
    };
    let mut document = JsonApiDocument {
        data: Some(PrimaryData::One(resource)),
        included: (include_requested || !included.is_empty()).then_some(included),
        ..JsonApiDocument::default()
    };
    if document.validate_response().is_err() {
        let sparse_fieldset_exception_applies = document.included.is_some()
            && plan
                .as_ref()
                .is_some_and(has_sparse_fieldset_include_relationship)
            && document
                .validate_response_with_sparse_fieldset_exception()
                .is_ok();
        if !sparse_fieldset_exception_applies {
            return adapter_error();
        }
    }
    if let Some(plan) = plan.as_ref() {
        if let (Some(PrimaryData::One(resource)), Some(fieldset)) = (
            document.data.as_mut(),
            plan.fieldsets.get(definition.type_name()),
        ) {
            apply_fieldset(resource, fieldset);
        }
        if let Some(included) = document.included.as_mut() {
            for resource in included {
                if let Some(fieldset) = plan.fieldsets.get(&resource.type_name) {
                    apply_fieldset(resource, fieldset);
                }
            }
        }
    }
    if state.links {
        apply_resource_links(&mut document, &resource_type, &id, query.as_deref());
    }
    respond_with_validated_document(StatusCode::OK, document)
}

#[allow(clippy::result_large_err)]
#[allow(clippy::too_many_lines)]
async fn get_related(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id, relationship_name)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = validate_request(&headers, None) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    let Some(relationship) = definition.relationship_by_name(&relationship_name).cloned() else {
        return protocol_error(
            StatusCode::NOT_FOUND,
            "relationship_not_found",
            "Relationship not found",
            Some(format!(
                "Relationship `{relationship_name}` is not registered on `{resource_type}`."
            )),
            None,
        );
    };
    if !relationship.allows(RelationshipPermission::RelatedRead) {
        return forbidden_error();
    }
    if relationship.cardinality().is_none() {
        return relationship_cardinality_required(&relationship_name);
    }
    let (Some(query_adapter), Some(pagination)) =
        (state.query_adapter.as_ref(), state.pagination.as_ref())
    else {
        return read_not_configured_error();
    };
    let read_query = match parse_read_query(query.as_deref().unwrap_or_default()) {
        Ok(query) => query,
        Err(error) => return query_parse_error(error),
    };
    let include_requested = !read_query.includes.is_empty();
    let target_type = relationship.target_type();
    // Related collections do not yet execute filter/sort/page; a resource plan
    // rejects those controls so they are never silently ignored.
    let plan = plan_resource_read(&state.registry, target_type, &read_query, pagination);
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return read_plan_error(error),
    };
    if let Some(limits) = &state.execution_limits
        && let Err(message) = limits.validate_read(&plan)
    {
        return limit_exceeded_error(message);
    }
    if !state
        .authorizer
        .authorize_query(definition, Some(&id), &plan, &headers)
        .await
    {
        return forbidden_error();
    }
    let result = match query_adapter
        .related(definition, &id, &relationship, &plan)
        .await
    {
        Ok(result) => result,
        Err(error) => return query_adapter_error(error),
    };
    let include_relationships = include_relationships_by_type(&plan);
    let target_definition = match state.registry.resource(target_type) {
        Ok(definition) => definition,
        Err(_) => return adapter_error(),
    };
    let target_relationships = include_relationships
        .get(target_type)
        .cloned()
        .unwrap_or_default();
    let mut document = match result {
        QueryRelatedResult::One(result) => match result {
            Some(QueryResourceResult { resource, included }) => {
                let root = match project_resource_with_includes(
                    target_definition,
                    &resource,
                    &target_relationships,
                ) {
                    Ok(root) => root,
                    Err(_) => return adapter_error(),
                };
                let included = match project_included(&state, included, &include_relationships) {
                    Ok(included) => included,
                    Err(_) => return adapter_error(),
                };
                JsonApiDocument {
                    data: Some(PrimaryData::One(root)),
                    included: (include_requested || !included.is_empty()).then_some(included),
                    ..JsonApiDocument::default()
                }
            }
            None => JsonApiDocument {
                data: Some(PrimaryData::Null),
                ..JsonApiDocument::default()
            },
        },
        QueryRelatedResult::Many(QueryCollectionResult {
            resources,
            included,
        }) => {
            let resources = match resources
                .iter()
                .map(|resource| {
                    project_resource_with_includes(
                        target_definition,
                        resource,
                        &target_relationships,
                    )
                })
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(resources) => resources,
                Err(_) => return adapter_error(),
            };
            let included = match project_included(&state, included, &include_relationships) {
                Ok(included) => included,
                Err(_) => return adapter_error(),
            };
            JsonApiDocument {
                data: Some(PrimaryData::Many(resources)),
                included: (include_requested || !included.is_empty()).then_some(included),
                ..JsonApiDocument::default()
            }
        }
    };
    if document.validate_response().is_err() {
        let sparse_fieldset_exception_applies = document.included.is_some()
            && has_sparse_fieldset_include_relationship(&plan)
            && document
                .validate_response_with_sparse_fieldset_exception()
                .is_ok();
        if !sparse_fieldset_exception_applies {
            return adapter_error();
        }
    }
    if let Some(PrimaryData::One(resource)) = document.data.as_mut()
        && let Some(fieldset) = plan.fieldsets.get(target_type)
    {
        apply_fieldset(resource, fieldset);
    }
    if let Some(PrimaryData::Many(resources)) = document.data.as_mut()
        && let Some(fieldset) = plan.fieldsets.get(target_type)
    {
        for resource in resources {
            apply_fieldset(resource, fieldset);
        }
    }
    if let Some(included) = document.included.as_mut() {
        for resource in included {
            if let Some(fieldset) = plan.fieldsets.get(&resource.type_name) {
                apply_fieldset(resource, fieldset);
            }
        }
    }
    if state.links {
        let path = format!(
            "/{}/{}/{}",
            encode_path_segment(&resource_type),
            encode_path_segment(&id),
            encode_path_segment(&relationship_name)
        );
        let mut links = serde_json::Map::new();
        links.insert(
            "self".to_owned(),
            Value::String(with_query(path, query.as_deref())),
        );
        document.links = Some(links);
        add_resource_self_links(&mut document);
    }
    respond_with_validated_document(StatusCode::OK, document)
}

fn project_included(
    state: &ApiState,
    included: Vec<AdapterIncludedResource>,
    include_relationships: &BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Result<Vec<ResourceObject>, ProjectionError> {
    included
        .iter()
        .map(|included| {
            let definition = state
                .registry
                .resource(&included.resource_type)
                .map_err(|_| ProjectionError::UnknownResourceType)?;
            project_resource_with_includes(
                definition,
                &included.resource,
                include_relationships
                    .get(&included.resource_type)
                    .unwrap_or(&std::collections::BTreeSet::new()),
            )
        })
        .collect()
}

async fn create_resource(
    State(state): State<Arc<ApiState>>,
    Path(resource_type): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = validate_mutation_request(&headers, query.as_deref(), true) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    let resource = match parse_mutation_document(&body) {
        Ok(resource) => resource,
        Err(response) => return response,
    };
    if let Err(response) = validate_mutation_resource_identity(&resource) {
        return response;
    }
    if resource.type_name != resource_type {
        return mutation_error(
            StatusCode::CONFLICT,
            "resource_type_mismatch",
            "Resource type does not match collection",
            Some(format!(
                "The request resource type `{}` does not match `{resource_type}`.",
                resource.type_name
            )),
            Some("/data/type"),
        );
    }
    if resource.id.is_some() {
        return mutation_error(
            StatusCode::FORBIDDEN,
            "client_generated_id_not_supported",
            "Client-generated IDs are not supported",
            Some("This endpoint does not accept a client-assigned resource ID.".to_owned()),
            Some("/data/id"),
        );
    }
    let changeset = match map_resource_changeset(definition, &resource, true) {
        Ok(changeset) => changeset,
        Err(response) => return response,
    };
    let outcome = match execute_mutation(
        &state,
        MutationAction::Create,
        definition,
        None,
        &headers,
        MutationCommand::Create { changeset },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    let MutationOutcome::Resource(record) = outcome else {
        return mutation_adapter_error(MutationAdapterError::Failed);
    };
    if record.id.is_empty() {
        return mutation_adapter_error(MutationAdapterError::Failed);
    }
    let resource = match project_resource(definition, &record) {
        Ok(resource) => resource,
        Err(_) => return mutation_adapter_error(MutationAdapterError::Failed),
    };
    let mut response = respond_with_validated_document(
        StatusCode::CREATED,
        JsonApiDocument {
            data: Some(PrimaryData::One(resource)),
            ..JsonApiDocument::default()
        },
    );
    if let Ok(location) = HeaderValue::from_str(&resource_location(&resource_type, &record.id)) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, location);
    }
    response
}

async fn update_resource(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = validate_mutation_request(&headers, query.as_deref(), true) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    let resource = match parse_mutation_document(&body) {
        Ok(resource) => resource,
        Err(response) => return response,
    };
    if let Err(response) = validate_mutation_resource_identity(&resource) {
        return response;
    }
    if resource.type_name != resource_type {
        return mutation_error(
            StatusCode::CONFLICT,
            "resource_type_mismatch",
            "Resource type does not match URL",
            Some(format!(
                "The request resource type `{}` does not match `{resource_type}`.",
                resource.type_name
            )),
            Some("/data/type"),
        );
    }
    if resource.id.as_deref() != Some(id.as_str()) || resource.lid.is_some() {
        return mutation_error(
            StatusCode::CONFLICT,
            "resource_id_mismatch",
            "Resource ID does not match URL",
            Some("The request resource ID must match the ID in the request URL.".to_owned()),
            Some("/data/id"),
        );
    }
    let changeset = match map_resource_changeset(definition, &resource, false) {
        Ok(changeset) => changeset,
        Err(response) => return response,
    };
    let outcome = match execute_mutation(
        &state,
        MutationAction::Update,
        definition,
        Some(&id),
        &headers,
        MutationCommand::Update {
            id: id.clone(),
            changeset,
        },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    let MutationOutcome::Resource(record) = outcome else {
        return mutation_adapter_error(MutationAdapterError::Failed);
    };
    if record.id != id {
        return mutation_adapter_error(MutationAdapterError::Failed);
    }
    resource_response(definition, record, StatusCode::OK)
}

async fn delete_resource(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = validate_mutation_request(&headers, query.as_deref(), false) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    match execute_mutation(
        &state,
        MutationAction::Delete,
        definition,
        Some(&id),
        &headers,
        MutationCommand::Delete { id: id.clone() },
    )
    .await
    {
        Ok(MutationOutcome::Deleted) => empty_success_response(),
        Ok(_) => mutation_adapter_error(MutationAdapterError::Failed),
        Err(response) => response,
    }
}

async fn get_relationship(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id, relationship_name)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = validate_mutation_request(&headers, query.as_deref(), false) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    let relationship = match declared_relationship(definition, &relationship_name) {
        Ok(relationship) => relationship,
        Err(response) => return response,
    };
    if !relationship.allows(RelationshipPermission::LinkageRead) {
        return forbidden_error();
    }
    let Some(cardinality) = relationship.cardinality() else {
        return relationship_cardinality_required(&relationship_name);
    };
    match execute_mutation(
        &state,
        MutationAction::ReadRelationship,
        definition,
        Some(&id),
        &headers,
        MutationCommand::ReadRelationship {
            id: id.clone(),
            relationship: relationship.clone(),
        },
    )
    .await
    {
        Ok(MutationOutcome::Relationship(data)) => {
            if !relationship_data_matches_cardinality(&data, cardinality)
                || !relationship_data_matches_target(&data, relationship.target_type())
            {
                return mutation_adapter_error(MutationAdapterError::Failed);
            }
            relationship_response(data)
        }
        Ok(_) => mutation_adapter_error(MutationAdapterError::Failed),
        Err(response) => response,
    }
}

async fn replace_relationship(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id, relationship_name)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    mutate_relationship(
        state,
        resource_type,
        id,
        relationship_name,
        query,
        headers,
        body,
        RelationshipHttpMethod::Replace,
    )
    .await
}

async fn add_relationship_members(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id, relationship_name)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    mutate_relationship(
        state,
        resource_type,
        id,
        relationship_name,
        query,
        headers,
        body,
        RelationshipHttpMethod::Add,
    )
    .await
}

async fn remove_relationship_members(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id, relationship_name)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    mutate_relationship(
        state,
        resource_type,
        id,
        relationship_name,
        query,
        headers,
        body,
        RelationshipHttpMethod::Remove,
    )
    .await
}

#[derive(Clone, Copy)]
enum RelationshipHttpMethod {
    Replace,
    Add,
    Remove,
}

#[allow(clippy::too_many_arguments)]
async fn mutate_relationship(
    state: Arc<ApiState>,
    resource_type: String,
    id: String,
    relationship_name: String,
    query: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    method: RelationshipHttpMethod,
) -> Response {
    if let Err(response) = validate_mutation_request(&headers, query.as_deref(), true) {
        return response;
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => return unknown_resource_type(&resource_type),
    };
    let relationship = match declared_relationship(definition, &relationship_name) {
        Ok(relationship) => relationship,
        Err(response) => return response,
    };
    let Some(cardinality) = relationship.cardinality() else {
        return relationship_cardinality_required(&relationship_name);
    };
    let permission = match method {
        RelationshipHttpMethod::Replace => RelationshipPermission::BaseReplace,
        RelationshipHttpMethod::Add => RelationshipPermission::BaseAdd,
        RelationshipHttpMethod::Remove => RelationshipPermission::BaseRemove,
    };
    if !relationship.allows(permission) {
        return forbidden_error();
    }
    if !matches!(method, RelationshipHttpMethod::Replace)
        && cardinality != RelationshipCardinality::ToMany
    {
        return unsupported_relationship_operation(&relationship_name);
    }
    let data = match parse_relationship_document(&body, cardinality, relationship.target_type()) {
        Ok(data) => data,
        Err(response) => return response,
    };
    let (action, mutation) = match (method, data) {
        (RelationshipHttpMethod::Replace, data) => (
            MutationAction::ReplaceRelationship,
            RelationshipMutation::Replace(data),
        ),
        (RelationshipHttpMethod::Add, RelationshipData::Many(identifiers)) => (
            MutationAction::AddRelationshipMembers,
            RelationshipMutation::Add(identifiers),
        ),
        (RelationshipHttpMethod::Remove, RelationshipData::Many(identifiers)) => (
            MutationAction::RemoveRelationshipMembers,
            RelationshipMutation::Remove(identifiers),
        ),
        _ => return unsupported_relationship_operation(&relationship_name),
    };
    match execute_mutation(
        &state,
        action,
        definition,
        Some(&id),
        &headers,
        MutationCommand::ModifyRelationship {
            id: id.clone(),
            relationship: relationship.clone(),
            mutation,
        },
    )
    .await
    {
        Ok(MutationOutcome::Relationship(data)) => {
            if !relationship_data_matches_cardinality(&data, cardinality)
                || !relationship_data_matches_target(&data, relationship.target_type())
            {
                return mutation_adapter_error(MutationAdapterError::Failed);
            }
            relationship_response(data)
        }
        Ok(_) => mutation_adapter_error(MutationAdapterError::Failed),
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
fn validate_mutation_request(
    headers: &HeaderMap,
    query: Option<&str>,
    requires_body: bool,
) -> Result<(), Response> {
    if !accepts_jsonapi(headers) {
        return Err(request_error_response(
            RequestValidationError::NotAcceptable,
        ));
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        return Err(query_parse_error(QueryParseError {
            parameter: first_query_parameter(query).unwrap_or_else(|| "query".to_owned()),
            detail: "query parameters are not supported on mutation routes".to_owned(),
        }));
    }
    if requires_body || headers.contains_key(CONTENT_TYPE) {
        validate_jsonapi_content_type(headers)?;
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_jsonapi_content_type(headers: &HeaderMap) -> Result<(), Response> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return Err(unsupported_media_type(
            "a JSON:API Content-Type header is required",
        ));
    };
    if values.next().is_some() {
        return Err(unsupported_media_type(
            "multiple Content-Type header values are not supported",
        ));
    }
    let Ok(value) = value.to_str() else {
        return Err(unsupported_media_type("the Content-Type header is invalid"));
    };
    let parameters = media::parse_jsonapi_content_type(value).map_err(unsupported_media_type)?;
    if !parameters.extensions.is_empty() {
        return Err(unsupported_media_type(
            "this endpoint does not support JSON:API extensions",
        ));
    }
    Ok(())
}

fn unsupported_media_type(detail: &str) -> Response {
    protocol_error(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        "Unsupported media type",
        Some(detail.to_owned()),
        None,
    )
}

#[allow(clippy::result_large_err)]
fn parse_mutation_document(body: &[u8]) -> Result<ResourceObject, Response> {
    let value = parse_unique_members(body).map_err(|error| {
        mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid JSON:API document",
            Some(format!("The request document could not be parsed: {error}")),
            None,
        )
    })?;
    let document = serde_json::from_value::<JsonApiDocument>(value).map_err(|error| {
        mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid JSON:API document",
            Some(format!("The request document could not be parsed: {error}")),
            None,
        )
    })?;
    if document.errors.is_some() || document.included.is_some() || document.data.is_none() {
        let pointer = if document.errors.is_some() {
            "/errors"
        } else if document.included.is_some() {
            "/included"
        } else {
            ""
        };
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid mutation document",
            Some("A mutation request must contain one primary resource in `data`.".to_owned()),
            Some(pointer),
        ));
    }
    match document.data {
        Some(PrimaryData::One(resource)) => Ok(resource),
        _ => Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid mutation document",
            Some("A mutation request must contain one resource object in `data`.".to_owned()),
            Some("/data"),
        )),
    }
}

#[allow(clippy::result_large_err)]
fn map_resource_changeset(
    definition: &ResourceDefinition,
    resource: &ResourceObject,
    create: bool,
) -> Result<ResourceMutationChangeset, Response> {
    if !definition.allows(if create {
        ResourcePermission::Create
    } else {
        ResourcePermission::Update
    }) {
        return Err(forbidden_error());
    }
    let mut changeset = ResourceMutationChangeset::default();
    if let Some(attributes) = &resource.attributes {
        for (public_name, value) in attributes {
            let Some(mapping) = definition.attribute_by_name(public_name) else {
                return Err(unknown_mutation_field(
                    public_name,
                    &format!("/data/attributes/{}", escape_json_pointer(public_name)),
                ));
            };
            if !mapping.allows(if create {
                AttributePermission::Create
            } else {
                AttributePermission::Update
            }) {
                return Err(forbidden_error());
            }
            changeset
                .attributes
                .insert(mapping.model_field().to_owned(), value.clone());
        }
    }
    if let Some(relationships) = &resource.relationships {
        for (public_name, relationship) in relationships {
            let Some(mapping) = definition.relationship_by_name(public_name) else {
                return Err(unknown_mutation_field(
                    public_name,
                    &format!("/data/relationships/{}", escape_json_pointer(public_name)),
                ));
            };
            if !mapping.allows(if create {
                RelationshipPermission::ResourceCreate
            } else {
                RelationshipPermission::ResourceUpdate
            }) {
                return Err(forbidden_error());
            }
            let Some(data) = relationship.data.clone() else {
                return Err(mutation_error(
                    StatusCode::BAD_REQUEST,
                    "relationship_data_required",
                    "Relationship data is required",
                    Some("Mutated relationships must include a `data` member.".to_owned()),
                    Some(&format!(
                        "/data/relationships/{}",
                        escape_json_pointer(public_name)
                    )),
                ));
            };
            let Some(cardinality) = mapping.cardinality() else {
                return Err(relationship_cardinality_required(public_name));
            };
            if !relationship_data_matches_cardinality(&data, cardinality) {
                return Err(mutation_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_relationship_linkage",
                    "Relationship linkage has the wrong shape",
                    Some(
                        "The linkage shape does not match the declared relationship cardinality."
                            .to_owned(),
                    ),
                    Some(&format!(
                        "/data/relationships/{}/data",
                        escape_json_pointer(public_name)
                    )),
                ));
            }
            if !relationship_data_matches_target(&data, mapping.target_type()) {
                return Err(mutation_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_relationship_linkage",
                    "Relationship linkage type is invalid",
                    Some(format!(
                        "All linkage must identify resources of type `{}`.",
                        mapping.target_type()
                    )),
                    Some(&format!(
                        "/data/relationships/{}/data",
                        escape_json_pointer(public_name)
                    )),
                ));
            }
            changeset
                .relationships
                .insert(mapping.model_field().to_owned(), data);
        }
    }
    Ok(changeset)
}

#[allow(clippy::result_large_err)]
fn validate_mutation_resource_identity(resource: &ResourceObject) -> Result<(), Response> {
    if resource.id.is_some() && resource.lid.is_some() {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_resource_identity",
            "Resource identity is invalid",
            Some("A resource object must not contain both `id` and `lid`.".to_owned()),
            Some("/data"),
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn parse_relationship_document(
    body: &[u8],
    cardinality: RelationshipCardinality,
    target_type: &str,
) -> Result<RelationshipData, Response> {
    let document = parse_unique_members(body).map_err(|error| {
        mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid JSON:API document",
            Some(format!("The request document could not be parsed: {error}")),
            None,
        )
    })?;
    let Some(object) = document.as_object() else {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid JSON:API document",
            Some("A JSON:API document must be an object.".to_owned()),
            Some(""),
        ));
    };
    let Some(raw_data) = object.get("data") else {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "relationship_data_required",
            "Relationship data is required",
            Some("A relationship mutation document must contain `data`.".to_owned()),
            Some(""),
        ));
    };
    if object.contains_key("errors") || object.contains_key("included") {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_document",
            "Invalid relationship document",
            Some(
                "A relationship mutation document must not contain `errors` or `included`."
                    .to_owned(),
            ),
            Some("/data"),
        ));
    }
    let data = serde_json::from_value::<RelationshipData>(raw_data.clone()).map_err(|error| {
        mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_relationship_linkage",
            "Invalid relationship linkage",
            Some(error.to_string()),
            Some("/data"),
        )
    })?;
    if !relationship_data_matches_cardinality(&data, cardinality) {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_relationship_linkage",
            "Relationship linkage has the wrong shape",
            Some(
                "The linkage shape does not match the declared relationship cardinality."
                    .to_owned(),
            ),
            Some("/data"),
        ));
    }
    if !relationship_data_matches_target(&data, target_type) {
        return Err(mutation_error(
            StatusCode::BAD_REQUEST,
            "invalid_relationship_linkage",
            "Relationship linkage type is invalid",
            Some(format!(
                "All linkage must identify resources of type `{target_type}`."
            )),
            Some("/data"),
        ));
    }
    Ok(data)
}

fn relationship_data_matches_cardinality(
    data: &RelationshipData,
    cardinality: RelationshipCardinality,
) -> bool {
    matches!(
        (cardinality, data),
        (
            RelationshipCardinality::ToOne,
            RelationshipData::Null | RelationshipData::One(_)
        ) | (RelationshipCardinality::ToMany, RelationshipData::Many(_))
    )
}

fn relationship_data_matches_target(data: &RelationshipData, target_type: &str) -> bool {
    let valid_identifier = |identifier: &crate::document::ResourceIdentifier| {
        identifier.type_name == target_type && identifier.id.is_some() && identifier.lid.is_none()
    };
    match data {
        RelationshipData::Null => true,
        RelationshipData::One(identifier) => valid_identifier(identifier),
        RelationshipData::Many(identifiers) => identifiers.iter().all(valid_identifier),
    }
}

#[allow(clippy::result_large_err)]
fn declared_relationship(
    definition: &ResourceDefinition,
    relationship_name: &str,
) -> Result<RelationshipMapping, Response> {
    definition
        .relationship_by_name(relationship_name)
        .cloned()
        .ok_or_else(|| {
            unknown_mutation_field(
                relationship_name,
                &format!(
                    "/data/relationships/{}",
                    escape_json_pointer(relationship_name)
                ),
            )
        })
}

fn unknown_resource_type(resource_type: &str) -> Response {
    protocol_error(
        StatusCode::NOT_FOUND,
        "unknown_resource_type",
        "Resource type not found",
        Some(format!(
            "Resource type `{resource_type}` is not registered."
        )),
        None,
    )
}

fn relationship_cardinality_required(relationship_name: &str) -> Response {
    mutation_error(
        StatusCode::NOT_IMPLEMENTED,
        "relationship_cardinality_required",
        "Relationship operations are not configured",
        Some(format!(
            "Relationship `{relationship_name}` must declare to-one or to-many cardinality."
        )),
        None,
    )
}

fn unsupported_relationship_operation(relationship_name: &str) -> Response {
    mutation_error(
        StatusCode::FORBIDDEN,
        "unsupported_relationship_operation",
        "Relationship operation is not supported",
        Some(format!(
            "This operation is not supported for relationship `{relationship_name}`."
        )),
        None,
    )
}

fn unknown_mutation_field(field: &str, pointer: &str) -> Response {
    mutation_error(
        StatusCode::BAD_REQUEST,
        "unknown_field",
        "Unknown resource field",
        Some(format!("Field `{field}` is not registered.")),
        Some(pointer),
    )
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[allow(clippy::result_large_err)]
async fn execute_mutation(
    state: &ApiState,
    action: MutationAction,
    definition: &ResourceDefinition,
    resource_id: Option<&str>,
    headers: &HeaderMap,
    command: MutationCommand,
) -> Result<MutationOutcome, Response> {
    let permission = match action {
        MutationAction::Create => Some(ResourcePermission::Create),
        MutationAction::Update => Some(ResourcePermission::Update),
        MutationAction::Delete => Some(ResourcePermission::Delete),
        MutationAction::ReadRelationship
        | MutationAction::ReplaceRelationship
        | MutationAction::AddRelationshipMembers
        | MutationAction::RemoveRelationshipMembers => None,
    };
    if permission.is_some_and(|permission| !definition.allows(permission)) {
        return Err(forbidden_error());
    }
    state
        .authorizer
        .validate_mutation_limits(action, definition, &command)
        .map_err(limit_exceeded_error)?;
    if let Some(limits) = &state.execution_limits {
        limits
            .validate_mutation(&command)
            .map_err(limit_exceeded_error)?;
    }
    if !state
        .authorizer
        .authorize_mutation(action, definition, resource_id, &command, headers)
        .await
    {
        return Err(forbidden_error());
    }
    let Some(adapter) = state.mutation_adapter.as_ref() else {
        return Err(mutation_error(
            StatusCode::NOT_IMPLEMENTED,
            "mutation_not_supported",
            "Mutations are not configured",
            Some("No base mutation adapter is configured for this router.".to_owned()),
            None,
        ));
    };
    adapter
        .execute(definition, command)
        .await
        .map_err(mutation_adapter_error)
}

fn resource_response(
    definition: &ResourceDefinition,
    record: AdapterResource,
    status: StatusCode,
) -> Response {
    match project_resource(definition, &record) {
        Ok(resource) => respond_with_validated_document(
            status,
            JsonApiDocument {
                data: Some(PrimaryData::One(resource)),
                ..JsonApiDocument::default()
            },
        ),
        Err(_) => mutation_adapter_error(MutationAdapterError::Failed),
    }
}

fn relationship_response(data: RelationshipData) -> Response {
    let mut response = Json(serde_json::json!({"data": data})).into_response();
    set_jsonapi_headers(&mut response);
    response
}

fn empty_success_response() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept"));
    response
}

fn resource_location(resource_type: &str, id: &str) -> String {
    format!(
        "/{}/{}",
        encode_path_segment(resource_type),
        encode_path_segment(id)
    )
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn mutation_adapter_error(error: MutationAdapterError) -> Response {
    match error {
        MutationAdapterError::NotFound => protocol_error(
            StatusCode::NOT_FOUND,
            "resource_not_found",
            "Resource not found",
            Some("The addressed resource does not exist.".to_owned()),
            None,
        ),
        MutationAdapterError::RelatedResourceNotFound => protocol_error(
            StatusCode::NOT_FOUND,
            "related_resource_not_found",
            "Related resource not found",
            Some("A resource referenced by the mutation does not exist.".to_owned()),
            None,
        ),
        MutationAdapterError::Conflict => protocol_error(
            StatusCode::CONFLICT,
            "resource_conflict",
            "Resource mutation conflict",
            Some("The requested mutation conflicts with the current state.".to_owned()),
            None,
        ),
        MutationAdapterError::Unsupported => protocol_error(
            StatusCode::FORBIDDEN,
            "unsupported_operation",
            "Operation is not supported",
            Some("The application does not support this operation.".to_owned()),
            None,
        ),
        MutationAdapterError::Failed => protocol_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "mutation_failed",
            "Resource mutation failed",
            Some("The resource could not be changed.".to_owned()),
            None,
        ),
    }
}

fn mutation_error(
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: Option<String>,
    pointer: Option<&str>,
) -> Response {
    let error = ErrorObject {
        status: Some(status.as_u16().to_string()),
        code: Some(code.to_owned()),
        title: Some(title.to_owned()),
        detail,
        source: pointer.map(|pointer| ErrorSource {
            pointer: Some(pointer.to_owned()),
            ..ErrorSource::default()
        }),
        ..ErrorObject::default()
    };
    let mut response = (
        status,
        Json(JsonApiDocument {
            errors: Some(vec![error]),
            ..JsonApiDocument::default()
        }),
    )
        .into_response();
    set_jsonapi_headers(&mut response);
    response
}

struct QueryParseError {
    parameter: String,
    detail: String,
}

fn parse_read_query(query: &str) -> Result<ReadQuery, QueryParseError> {
    let mut parsed = ReadQuery::default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = decode_query_component(raw_name).map_err(|detail| QueryParseError {
            parameter: "query".to_owned(),
            detail: detail.to_owned(),
        })?;
        let value = decode_query_component(raw_value).map_err(|detail| QueryParseError {
            parameter: name.clone(),
            detail: detail.to_owned(),
        })?;
        match name.as_str() {
            "filter" => parsed.filters.push(value),
            "sort" => set_unique_query_value(&mut parsed.sort, &name, value)?,
            "page[number]" => set_unique_query_value(&mut parsed.page_number, &name, value)?,
            "page[size]" => set_unique_query_value(&mut parsed.page_size, &name, value)?,
            "include" => parsed.includes.push(value),
            _ => {
                if let Some(resource_type) = name
                    .strip_prefix("fields[")
                    .and_then(|name| name.strip_suffix(']'))
                {
                    if resource_type.is_empty() {
                        return Err(QueryParseError {
                            parameter: name,
                            detail: "fieldset resource type must not be empty".to_owned(),
                        });
                    }
                    if parsed
                        .fieldsets
                        .insert(resource_type.to_owned(), value)
                        .is_some()
                    {
                        return Err(QueryParseError {
                            parameter: name,
                            detail: "fieldset parameter must not be repeated".to_owned(),
                        });
                    }
                } else {
                    parsed.unsupported_parameters.push(name);
                }
            }
        }
    }
    Ok(parsed)
}

fn set_unique_query_value(
    destination: &mut Option<String>,
    parameter: &str,
    value: String,
) -> Result<(), QueryParseError> {
    if destination.replace(value).is_some() {
        return Err(QueryParseError {
            parameter: parameter.to_owned(),
            detail: "query parameter must not be repeated".to_owned(),
        });
    }
    Ok(())
}

fn decode_query_component(input: &str) -> Result<String, &'static str> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err("invalid percent-encoding");
                }
                let high = hex_digit(bytes[index + 1]).ok_or("invalid percent-encoding")?;
                let low = hex_digit(bytes[index + 2]).ok_or("invalid percent-encoding")?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| "query parameter is not valid UTF-8")
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn query_parse_error(error: QueryParseError) -> Response {
    protocol_error(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "Invalid query parameter",
        Some(error.detail),
        Some(error.parameter),
    )
}

fn read_plan_error(error: ReadPlanError) -> Response {
    if matches!(&error, ReadPlanError::IncludeNotEnabled { .. }) {
        return forbidden_error();
    }
    let parameter = match &error {
        ReadPlanError::UnsupportedQueryParameters(parameters) => parameters.first().cloned(),
        ReadPlanError::Filter(_) => Some("filter".to_owned()),
        ReadPlanError::InvalidSort(_)
        | ReadPlanError::DuplicateSortField(_)
        | ReadPlanError::UnknownSortAttribute { .. }
        | ReadPlanError::AttributeNotSortable { .. } => Some("sort".to_owned()),
        ReadPlanError::UnknownFieldsetField { resource_type, .. }
        | ReadPlanError::InvalidFieldset { resource_type, .. } => {
            Some(format!("fields[{resource_type}]"))
        }
        ReadPlanError::InvalidIncludePath(_)
        | ReadPlanError::UnknownRelationship { .. }
        | ReadPlanError::IncludeNotEnabled { .. } => Some("include".to_owned()),
        ReadPlanError::InvalidPageParameter { parameter, .. } => Some((*parameter).to_owned()),
        ReadPlanError::PageSizeExceedsMaximum { .. } => Some("page[size]".to_owned()),
        ReadPlanError::PageOffsetOverflow => Some("page[number]".to_owned()),
        ReadPlanError::UnknownResourceType(resource_type) => {
            Some(format!("fields[{resource_type}]"))
        }
        ReadPlanError::InvalidPaginationConfig(_)
        | ReadPlanError::PageOffsetExceedsMaximum { .. } => None,
    };
    protocol_error(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "Invalid query",
        Some(error.to_string()),
        parameter,
    )
}

enum RequestValidationError {
    NotAcceptable,
    UnsupportedQuery(Option<String>),
}

#[allow(clippy::result_large_err)]
fn validate_request(headers: &HeaderMap, query: Option<&str>) -> Result<(), Response> {
    if !accepts_jsonapi(headers) {
        return Err(request_error_response(
            RequestValidationError::NotAcceptable,
        ));
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let parameter = first_query_parameter(query);
        return Err(request_error_response(
            RequestValidationError::UnsupportedQuery(parameter),
        ));
    }
    if headers.contains_key(CONTENT_TYPE) {
        validate_jsonapi_content_type(headers)?;
    }
    Ok(())
}

fn request_error_response(error: RequestValidationError) -> Response {
    match error {
        RequestValidationError::NotAcceptable => protocol_error(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            "JSON:API response is not acceptable",
            Some(format!(
                "This endpoint only produces `{JSONAPI_MEDIA_TYPE}`."
            )),
            None,
        ),
        RequestValidationError::UnsupportedQuery(parameter) => protocol_error(
            StatusCode::BAD_REQUEST,
            "unsupported_query",
            "Unsupported query parameter",
            Some("This endpoint does not support query parameters.".to_owned()),
            parameter,
        ),
    }
}

fn first_query_parameter(query: &str) -> Option<String> {
    query
        .split('&')
        .filter(|component| !component.is_empty())
        .map(|component| component.split('=').next().unwrap_or_default())
        .find(|name| !name.is_empty())
        .map(str::to_owned)
}

fn accepts_jsonapi(headers: &HeaderMap) -> bool {
    media::accepts_jsonapi(headers, |extensions| extensions.is_empty())
}

fn apply_fieldset(resource: &mut ResourceObject, fieldset: &[PlannedField]) {
    let attributes = fieldset
        .iter()
        .filter_map(|field| match field {
            PlannedField::Attribute { public_name, .. } => Some(public_name.as_str()),
            PlannedField::Relationship { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let relationships = fieldset
        .iter()
        .filter_map(|field| match field {
            PlannedField::Attribute { .. } => None,
            PlannedField::Relationship { public_name, .. } => Some(public_name.as_str()),
        })
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(resource_attributes) = resource.attributes.as_mut() {
        resource_attributes.retain(|name, _| attributes.contains(name.as_str()));
        if resource_attributes.is_empty() {
            resource.attributes = None;
        }
    }
    if let Some(resource_relationships) = resource.relationships.as_mut() {
        resource_relationships.retain(|name, _| relationships.contains(name.as_str()));
        if resource_relationships.is_empty() {
            resource.relationships = None;
        }
    }
}

fn has_sparse_fieldset_include_relationship(plan: &ReadPlan) -> bool {
    fn has_hidden_relationship(
        resource_type: &str,
        includes: &[IncludeNode],
        fieldsets: &BTreeMap<String, Vec<PlannedField>>,
    ) -> bool {
        includes.iter().any(|include| {
            let relationship_is_hidden = fieldsets.get(resource_type).is_some_and(|fieldset| {
                !fieldset.iter().any(|field| {
                    matches!(
                        field,
                        PlannedField::Relationship { public_name, .. }
                            if public_name == &include.public_name
                    )
                })
            });
            relationship_is_hidden
                || has_hidden_relationship(&include.target_type, &include.children, fieldsets)
        })
    }

    has_hidden_relationship(&plan.resource_type, &plan.includes, &plan.fieldsets)
}

fn respond_with_validated_document(status: StatusCode, document: JsonApiDocument) -> Response {
    let mut response = (status, Json(document)).into_response();
    set_jsonapi_headers(&mut response);
    response
}

fn with_query(path: String, raw_query: Option<&str>) -> String {
    match raw_query.filter(|query| !query.is_empty()) {
        Some(query) => format!("{path}?{query}"),
        None => path,
    }
}

fn resource_self_url(resource_type: &str, id: &str) -> String {
    format!(
        "/{}/{}",
        encode_path_segment(resource_type),
        encode_path_segment(id)
    )
}

fn add_resource_self_links(document: &mut JsonApiDocument) {
    fn add(resource: &mut ResourceObject) {
        let url = resource_self_url(
            &resource.type_name,
            resource.id.as_deref().unwrap_or_default(),
        );
        resource
            .links
            .get_or_insert_with(serde_json::Map::new)
            .insert("self".to_owned(), Value::String(url));
    }
    match document.data.as_mut() {
        Some(PrimaryData::One(resource)) => add(resource),
        Some(PrimaryData::Many(resources)) => resources.iter_mut().for_each(add),
        Some(PrimaryData::Null) | None => {}
    }
    if let Some(included) = document.included.as_mut() {
        included.iter_mut().for_each(add);
    }
}

fn apply_collection_links(
    document: &mut JsonApiDocument,
    resource_type: &str,
    raw_query: Option<&str>,
    page: Option<crate::query::Page>,
    returned: u64,
) {
    add_resource_self_links(document);
    let path = format!("/{}", encode_path_segment(resource_type));
    let mut links = serde_json::Map::new();
    links.insert(
        "self".to_owned(),
        Value::String(with_query(path.clone(), raw_query)),
    );
    if let Some(page) = page {
        links.insert(
            "first".to_owned(),
            Value::String(page_url(&path, raw_query, 1, page.size)),
        );
        if page.number > 1 {
            links.insert(
                "prev".to_owned(),
                Value::String(page_url(&path, raw_query, page.number - 1, page.size)),
            );
        }
        if returned >= page.size {
            links.insert(
                "next".to_owned(),
                Value::String(page_url(&path, raw_query, page.number + 1, page.size)),
            );
        }
        links.insert("last".to_owned(), Value::Null);
    }
    document.links = Some(links);
}

fn apply_resource_links(
    document: &mut JsonApiDocument,
    resource_type: &str,
    id: &str,
    raw_query: Option<&str>,
) {
    add_resource_self_links(document);
    let mut links = serde_json::Map::new();
    links.insert(
        "self".to_owned(),
        Value::String(with_query(resource_self_url(resource_type, id), raw_query)),
    );
    document.links = Some(links);
}

fn page_url(path: &str, raw_query: Option<&str>, number: u64, size: u64) -> String {
    let mut pairs = Vec::new();
    if let Some(query) = raw_query {
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let name = pair.split('=').next().unwrap_or_default();
            let decoded = decode_query_component(name).unwrap_or_else(|_| name.to_owned());
            if decoded == "page[number]" || decoded == "page[size]" {
                continue;
            }
            pairs.push(pair.to_owned());
        }
    }
    pairs.push(format!("page%5Bnumber%5D={number}"));
    pairs.push(format!("page%5Bsize%5D={size}"));
    format!("{path}?{}", pairs.join("&"))
}

fn forbidden_error() -> Response {
    protocol_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "Access denied",
        Some("Access to this resource is not permitted.".to_owned()),
        None,
    )
}

fn limit_exceeded_error(message: String) -> Response {
    protocol_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "resource_limit",
        "Mutation exceeds configured limits",
        Some(message),
        None,
    )
}

fn adapter_error() -> Response {
    protocol_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "read_failed",
        "Resource read failed",
        Some("The resource could not be loaded.".to_owned()),
        None,
    )
}

fn read_not_configured_error() -> Response {
    protocol_error(
        StatusCode::NOT_IMPLEMENTED,
        "read_not_supported",
        "Reads are not configured",
        Some("Enable a read or query adapter before serving read routes.".to_owned()),
        None,
    )
}

fn query_adapter_error(error: QueryAdapterError) -> Response {
    match error {
        QueryAdapterError::ReadFailed => adapter_error(),
        QueryAdapterError::NotAuthorized => forbidden_error(),
        QueryAdapterError::ResourceReadUnsupported => protocol_error(
            StatusCode::NOT_IMPLEMENTED,
            "resource_query_not_supported",
            "Single-resource queries are not implemented",
            Some("The configured query adapter cannot execute single-resource reads.".to_owned()),
            None,
        ),
        QueryAdapterError::LimitExceeded => protocol_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "resource_limit",
            "Query exceeds configured limits",
            Some("The requested query exceeds the server's configured limits.".to_owned()),
            None,
        ),
    }
}

fn protocol_error(
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: Option<String>,
    parameter: Option<String>,
) -> Response {
    let error = ErrorObject {
        status: Some(status.as_u16().to_string()),
        code: Some(code.to_owned()),
        title: Some(title.to_owned()),
        detail,
        source: parameter.map(|parameter| ErrorSource {
            parameter: Some(parameter),
            ..ErrorSource::default()
        }),
        ..ErrorObject::default()
    };
    let mut response = (
        status,
        Json(JsonApiDocument {
            errors: Some(vec![error]),
            ..JsonApiDocument::default()
        }),
    )
        .into_response();
    set_jsonapi_headers(&mut response);
    response
}

fn set_jsonapi_headers(response: &mut Response<Body>) {
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(JSONAPI_MEDIA_TYPE));
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept"));
}

#[cfg(test)]
mod link_tests {
    use super::page_url;

    #[test]
    fn pagination_links_preserve_other_query_parameters() {
        assert_eq!(
            page_url(
                "/ports",
                Some("filter=x&page%5Bnumber%5D=3&include=owner"),
                2,
                10
            ),
            "/ports?filter=x&include=owner&page%5Bnumber%5D=2&page%5Bsize%5D=10"
        );
        assert_eq!(
            page_url("/ports", None, 1, 25),
            "/ports?page%5Bnumber%5D=1&page%5Bsize%5D=25"
        );
    }
}
