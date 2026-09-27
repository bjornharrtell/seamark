#![cfg(feature = "sqlite")]
#![allow(missing_docs)]

#[path = "support/query_cases.rs"]
mod query_cases;

#[path = "support/atomic_cases.rs"]
mod atomic_cases;

#[path = "support/string_identifier_cases.rs"]
mod string_identifier_cases;

#[path = "support/bigint_identifier_cases.rs"]
mod bigint_identifier_cases;

#[path = "support/uuid_identifier_cases.rs"]
mod uuid_identifier_cases;

#[path = "support/http_mutation_cases.rs"]
mod http_mutation_cases;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection,
    DatabaseTransaction, DbBackend, EntityTrait, QueryFilter, Schema, Set, Value,
};
use seamark::atomic::{
    AtomicExecutionError, AtomicOperationHandler, AtomicOperationOutcome, AtomicOperationsDocument,
    AtomicOperationsGuard, LocalIdMap, PlannedAtomicOperation, PlannedOperation,
    execute_atomic_operations, plan_atomic_operations,
};
use seamark::atomic_http;
use seamark::http::{
    self, AdapterError, AdapterIncludedResource, AdapterResource, QueryAdapterError,
    QueryCollectionResult, QueryResourceAdapter, QueryResourceResult, RequestAuthorizer,
    ResourceAdapter,
};
use seamark::query::{
    FilterExpression, FilterValue, IncludeNode, Page, PaginationConfig, PlannedField, ReadPlan,
    ReadQuery, SortDirection, SortField, plan_read, plan_resource_read,
};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::{
    IncludedResource, SeaOrmExecutionError, SeaOrmFilterValueCodec, SeaOrmIncludeLoader,
    SeaOrmMutationValueCodec, SeaOrmQueryExecutor, SeaOrmReadGuard,
};
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::json;
use tower::ServiceExt;

const ATOMIC_MEDIA_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

fn atomic_operations_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

mod port {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_ports")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub port_id: i32,
        pub title: String,
        pub berth_count: Option<i32>,
        pub depth_m: i32,
        pub active: bool,
        pub owner_id: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::person::Entity",
            from = "Column::OwnerId",
            to = "super::person::Column::PersonId"
        )]
        Owner,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod person {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_people")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub person_id: i32,
        pub display_name: String,
        pub private_note: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

fn registry() -> ResourceRegistry {
    ResourceRegistry::new([
        ResourceDefinition::new("ports", "port_id")
            .attribute("name", "title", true, true)
            .attribute("capacity", "berth_count", true, true)
            .attribute("depth", "depth_m", true, true)
            .attribute("active", "active", true, true)
            .relationship("owner", "owner_id", "people")
            .relationship("neighbors", "neighbor_ids", "ports"),
        ResourceDefinition::new("people", "person_id")
            .attribute("name", "display_name", false, true)
            .attribute("note", "private_note", false, false),
    ])
    .unwrap()
}

fn pagination() -> PaginationConfig {
    PaginationConfig::new(1, 2, Some(10), Some(100)).unwrap()
}

fn plan(query: &ReadQuery) -> ReadPlan {
    plan_read(&registry(), "ports", query, &pagination()).unwrap()
}

fn port_resource(model: &port::Model) -> AdapterResource {
    AdapterResource {
        id: model.port_id.to_string(),
        attributes: BTreeMap::from([
            ("title".to_owned(), json!(model.title)),
            ("berth_count".to_owned(), json!(model.berth_count)),
            ("depth_m".to_owned(), json!(model.depth_m)),
            ("active".to_owned(), json!(model.active)),
            ("unregistered".to_owned(), json!("must not leak")),
        ]),
        relationships: BTreeMap::from([
            (
                "owner_id".to_owned(),
                seamark::document::Relationship {
                    data: Some(match model.owner_id {
                        Some(owner_id) => seamark::document::RelationshipData::One(
                            seamark::document::ResourceIdentifier {
                                type_name: "people".to_owned(),
                                id: Some(owner_id.to_string()),
                                ..seamark::document::ResourceIdentifier::default()
                            },
                        ),
                        None => seamark::document::RelationshipData::Null,
                    }),
                    ..seamark::document::Relationship::default()
                },
            ),
            (
                "neighbor_ids".to_owned(),
                seamark::document::Relationship {
                    data: Some(seamark::document::RelationshipData::Many(
                        query_cases::neighbor_ids(model.port_id)
                            .iter()
                            .map(|id| seamark::document::ResourceIdentifier {
                                type_name: "ports".to_owned(),
                                id: Some(id.to_string()),
                                ..seamark::document::ResourceIdentifier::default()
                            })
                            .collect(),
                    )),
                    ..seamark::document::Relationship::default()
                },
            ),
        ]),
    }
}

struct PortCodec;

impl SeaOrmFilterValueCodec for PortCodec {
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "port_id" | "berth_count" | "depth_m" => value
                .parse::<i32>()
                .map(Value::from)
                .map_err(|error| error.to_string()),
            "active" => value
                .parse::<bool>()
                .map(Value::from)
                .map_err(|error| error.to_string()),
            _ => Ok(Value::from(value.to_owned())),
        }
    }
}

struct AllowGuard;

#[async_trait]
impl SeaOrmReadGuard for AllowGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        true
    }

    fn validate_limits(&self, _plan: &ReadPlan) -> Result<(), String> {
        Ok(())
    }
}

struct CountingGuard {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SeaOrmReadGuard for CountingGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn validate_limits(&self, _plan: &ReadPlan) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct BoundedGuard {
    authorization_calls: Arc<AtomicUsize>,
    limit_calls: Arc<AtomicUsize>,
    authorized: bool,
    maximum_page_size: u64,
    maximum_offset: u64,
}

#[async_trait]
impl SeaOrmReadGuard for BoundedGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        self.authorization_calls.fetch_add(1, Ordering::SeqCst);
        self.authorized
    }

    fn validate_limits(&self, plan: &ReadPlan) -> Result<(), String> {
        self.limit_calls.fetch_add(1, Ordering::SeqCst);
        if plan.page.size > self.maximum_page_size {
            return Err("page size limit exceeded".to_owned());
        }
        if plan.page.offset > self.maximum_offset {
            return Err("offset limit exceeded".to_owned());
        }
        Ok(())
    }
}

struct DenyGuard;

#[async_trait]
impl SeaOrmReadGuard for DenyGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        false
    }

    fn validate_limits(&self, _plan: &ReadPlan) -> Result<(), String> {
        Ok(())
    }
}

struct PortOwnerLoader;

#[async_trait]
impl SeaOrmIncludeLoader<port::Entity> for PortOwnerLoader {
    async fn load_included(
        &self,
        database: &DatabaseConnection,
        roots: &[port::Model],
        includes: &[IncludeNode],
        _fieldsets: &BTreeMap<String, Vec<seamark::query::PlannedField>>,
    ) -> Result<Vec<IncludedResource>, String> {
        let mut included = Vec::new();
        if includes
            .iter()
            .any(|include| include.public_name == "owner")
        {
            let ids = roots
                .iter()
                .filter_map(|root| root.owner_id)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let people = person::Entity::find()
                .filter(person::Column::PersonId.is_in(ids))
                .all(database)
                .await
                .map_err(|error| error.to_string())?;
            included.extend(people.into_iter().map(|model| IncludedResource {
                resource_type: "people".to_owned(),
                resource: AdapterResource {
                    id: model.person_id.to_string(),
                    attributes: BTreeMap::from([
                        ("display_name".to_owned(), json!(model.display_name)),
                        ("private_note".to_owned(), json!(model.private_note)),
                        ("unregistered".to_owned(), json!("must not leak")),
                    ]),
                    ..AdapterResource::default()
                },
            }));
        }
        if let Some(neighbors_include) = includes
            .iter()
            .find(|include| include.public_name == "neighbors")
        {
            let root_ids = roots
                .iter()
                .map(|root| root.port_id)
                .collect::<BTreeSet<_>>();
            let ids = roots
                .iter()
                .flat_map(|root| query_cases::neighbor_ids(root.port_id).iter().copied())
                .filter(|id| !root_ids.contains(id))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let neighbors = port::Entity::find()
                .filter(port::Column::PortId.is_in(ids))
                .all(database)
                .await
                .map_err(|error| error.to_string())?;
            let mut included_ids = root_ids;
            included_ids.extend(neighbors.iter().map(|model| model.port_id));
            included.extend(neighbors.iter().map(|model| IncludedResource {
                resource_type: "ports".to_owned(),
                resource: port_resource(model),
            }));
            if neighbors_include
                .children
                .iter()
                .any(|child| child.public_name == "neighbors")
            {
                let nested_ids = neighbors
                    .iter()
                    .flat_map(|root| query_cases::neighbor_ids(root.port_id).iter().copied())
                    .filter(|id| !included_ids.contains(id))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let nested_neighbors = port::Entity::find()
                    .filter(port::Column::PortId.is_in(nested_ids))
                    .all(database)
                    .await
                    .map_err(|error| error.to_string())?;
                included.extend(nested_neighbors.iter().map(|model| IncludedResource {
                    resource_type: "ports".to_owned(),
                    resource: port_resource(model),
                }));
            }
        }
        Ok(included)
    }
}

struct CountingPortOwnerLoader {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SeaOrmIncludeLoader<port::Entity> for CountingPortOwnerLoader {
    async fn load_included(
        &self,
        _database: &DatabaseConnection,
        _roots: &[port::Model],
        _includes: &[IncludeNode],
        _fieldsets: &BTreeMap<String, Vec<seamark::query::PlannedField>>,
    ) -> Result<Vec<IncludedResource>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

type PortQueryExecutor =
    SeaOrmQueryExecutor<port::Entity, fn(&port::Model) -> AdapterResource, PortCodec>;

struct PortHttpQueryAdapter {
    database: DatabaseConnection,
    executor: PortQueryExecutor,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl QueryResourceAdapter for PortHttpQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .executor
            .collection(&self.database, plan, &AllowGuard, Some(&PortOwnerLoader))
            .await
            .map_err(|error| match error {
                seamark::seaorm::SeaOrmExecutionError::NotAuthorized => {
                    QueryAdapterError::NotAuthorized
                }
                seamark::seaorm::SeaOrmExecutionError::LimitExceeded(_) => {
                    QueryAdapterError::LimitExceeded
                }
                _ => QueryAdapterError::ReadFailed,
            })?;
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
        _resource: &ResourceDefinition,
        id: &str,
        plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .executor
            .resource(
                &self.database,
                id,
                plan,
                &AllowGuard,
                Some(&PortOwnerLoader),
            )
            .await
            .map_err(|error| match error {
                seamark::seaorm::SeaOrmExecutionError::NotAuthorized => {
                    QueryAdapterError::NotAuthorized
                }
                seamark::seaorm::SeaOrmExecutionError::LimitExceeded(_) => {
                    QueryAdapterError::LimitExceeded
                }
                _ => QueryAdapterError::ReadFailed,
            })?;
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
}

struct SingleResourceProbeAdapter {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ResourceAdapter for SingleResourceProbeAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
    ) -> Result<Vec<AdapterResource>, AdapterError> {
        Ok(Vec::new())
    }

    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
    ) -> Result<Option<AdapterResource>, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

struct SingleResourceProbeAuthorizer {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RequestAuthorizer for SingleResourceProbeAuthorizer {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &HeaderMap,
    ) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        true
    }
}

async fn database() -> DatabaseConnection {
    Database::connect("sqlite::memory:").await.unwrap()
}

async fn create_tables(database: &DatabaseConnection) {
    let schema = Schema::new(DbBackend::Sqlite);
    for statement in [
        schema.create_table_from_entity(person::Entity),
        schema.create_table_from_entity(port::Entity),
    ] {
        database
            .execute(database.get_database_backend().build(&statement))
            .await
            .unwrap();
    }
}

async fn insert_fixtures(database: &DatabaseConnection) {
    for query_cases::PersonFixture { id, name, note } in query_cases::PEOPLE {
        person::ActiveModel {
            person_id: Set(id),
            display_name: Set(name.to_owned()),
            private_note: Set(note.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }
    for query_cases::PortFixture {
        id,
        name,
        capacity,
        depth,
        active,
        owner_id,
    } in query_cases::PORTS
    {
        port::ActiveModel {
            port_id: Set(id),
            title: Set(name.to_owned()),
            berth_count: Set(capacity),
            depth_m: Set(depth),
            active: Set(active),
            owner_id: Set(owner_id),
        }
        .insert(database)
        .await
        .unwrap();
    }
}

impl SeaOrmMutationValueCodec for PortCodec {
    fn encode_mutation_value(
        &self,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<Value, String> {
        match (field, value) {
            ("owner_id" | "berth_count", serde_json::Value::Null) => Ok(Value::Int(None)),
            ("port_id" | "person_id" | "owner_id", serde_json::Value::String(value)) => value
                .parse::<i32>()
                .map(|value| Value::Int(Some(value)))
                .map_err(|error| error.to_string()),
            ("berth_count" | "depth_m", serde_json::Value::Number(value)) => value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .map(|value| Value::Int(Some(value)))
                .ok_or_else(|| format!("invalid integer value `{value}` for `{field}`")),
            ("active", serde_json::Value::Bool(value)) => Ok(Value::from(*value)),
            ("title" | "display_name" | "private_note", serde_json::Value::String(value)) => {
                Ok(Value::from(value.clone()))
            }
            _ => Err(format!("unsupported value `{value}` for `{field}`")),
        }
    }

    fn decode_identifier(&self, field: &str, value: &Value) -> Result<String, String> {
        match (field, value) {
            ("port_id" | "person_id", Value::Int(Some(value))) => Ok(value.to_string()),
            _ => Err(format!("unsupported identifier `{value:?}` for `{field}`")),
        }
    }
}

fn mutation_dispatcher(registry: &ResourceRegistry) -> SeaOrmAtomicOperationDispatcher {
    let codec = Arc::new(PortCodec);
    let ports = SeaOrmResourceMutationHandler::<port::Entity, _>::new(
        registry,
        "ports",
        Arc::clone(&codec),
    )
    .unwrap();
    let people = SeaOrmResourceMutationHandler::<person::Entity, _>::new(
        registry,
        "people",
        Arc::clone(&codec),
    )
    .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> =
        vec![Arc::new(ports), Arc::new(people)];
    SeaOrmAtomicOperationDispatcher::new(executors)
}

async fn execute_sqlite_mutations(
    database: &DatabaseConnection,
    registry: &ResourceRegistry,
    request: serde_json::Value,
) -> Result<Vec<seamark::atomic::AtomicResult>, AtomicExecutionError> {
    let document: AtomicOperationsDocument = serde_json::from_value(request).unwrap();
    let operations = plan_atomic_operations(registry, &document).unwrap();
    execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowAtomicGuard,
        &mutation_dispatcher(registry),
    )
    .await
}

#[tokio::test]
async fn sqlite_query_executor_validates_include_trees_before_authorization_or_loading() {
    let database = database().await;
    let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::new(
        registry(),
        "ports",
        port_resource,
        PortCodec,
    )
    .unwrap();
    let invalid_pages = [
        (
            Page {
                number: 0,
                size: 1,
                offset: 0,
                limit: 1,
            },
            "page number must be positive",
        ),
        (
            Page {
                number: 1,
                size: 0,
                offset: 0,
                limit: 0,
            },
            "page size must be positive",
        ),
        (
            Page {
                number: 1,
                size: 1,
                offset: 0,
                limit: 0,
            },
            "page limit must be positive",
        ),
        (
            Page {
                number: 1,
                size: 1,
                offset: 0,
                limit: 2,
            },
            "page limit must match page size",
        ),
        (
            Page {
                number: u64::MAX,
                size: 2,
                offset: 0,
                limit: 2,
            },
            "page offset overflows",
        ),
        (
            Page {
                number: 2,
                size: 5,
                offset: 4,
                limit: 5,
            },
            "page offset does not match page number and size",
        ),
    ];
    let page_validation_calls = Arc::new(AtomicUsize::new(0));
    let page_guard = CountingGuard {
        calls: page_validation_calls.clone(),
    };
    for (page, expected_detail) in invalid_pages {
        let mut invalid_plan = plan(&ReadQuery::default());
        invalid_plan.page = page;
        assert!(matches!(
            executor
                .collection(&database, &invalid_plan, &page_guard, None)
                .await,
            Err(SeaOrmExecutionError::InvalidPagePlan(detail))
                if detail == expected_detail
        ));
    }
    assert_eq!(page_validation_calls.load(Ordering::SeqCst), 0);

    for (page, expected_detail) in [
        (
            Page {
                number: 1,
                size: 11,
                offset: 0,
                limit: 11,
            },
            "page size limit exceeded",
        ),
        (
            Page {
                number: 102,
                size: 1,
                offset: 101,
                limit: 1,
            },
            "offset limit exceeded",
        ),
    ] {
        let authorization_calls = Arc::new(AtomicUsize::new(0));
        let limit_calls = Arc::new(AtomicUsize::new(0));
        let bounded_guard = BoundedGuard {
            authorization_calls: authorization_calls.clone(),
            limit_calls: limit_calls.clone(),
            authorized: false,
            maximum_page_size: 10,
            maximum_offset: 100,
        };
        let mut over_limit_plan = plan(&ReadQuery::default());
        over_limit_plan.page = page;
        assert!(matches!(
            executor
                .collection(&database, &over_limit_plan, &bounded_guard, None)
                .await,
            Err(SeaOrmExecutionError::LimitExceeded(message))
                if message == expected_detail
        ));
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 0);
        assert_eq!(limit_calls.load(Ordering::SeqCst), 1);
    }

    let invalid_include_cases = vec![
        (
            vec![IncludeNode {
                public_name: "secret".to_owned(),
                model_field: "owner_id".to_owned(),
                target_type: "people".to_owned(),
                children: Vec::new(),
            }],
            "ports",
            "secret",
        ),
        (
            vec![IncludeNode {
                public_name: "owner".to_owned(),
                model_field: "secret".to_owned(),
                target_type: "people".to_owned(),
                children: Vec::new(),
            }],
            "ports",
            "owner",
        ),
        (
            vec![IncludeNode {
                public_name: "owner".to_owned(),
                model_field: "owner_id".to_owned(),
                target_type: "ports".to_owned(),
                children: Vec::new(),
            }],
            "ports",
            "owner",
        ),
        (
            vec![IncludeNode {
                public_name: "neighbors".to_owned(),
                model_field: "neighbor_ids".to_owned(),
                target_type: "ports".to_owned(),
                children: vec![IncludeNode {
                    public_name: "owner".to_owned(),
                    model_field: "owner_id".to_owned(),
                    target_type: "ports".to_owned(),
                    children: Vec::new(),
                }],
            }],
            "ports",
            "owner",
        ),
    ];
    let validation_calls = Arc::new(AtomicUsize::new(0));
    let loader_calls = Arc::new(AtomicUsize::new(0));
    let counting_guard = CountingGuard {
        calls: validation_calls.clone(),
    };
    let counting_loader = CountingPortOwnerLoader {
        calls: loader_calls.clone(),
    };
    for (includes, expected_resource, expected_relationship) in invalid_include_cases {
        let mut invalid_plan = plan(&ReadQuery::default());
        invalid_plan.includes = includes;
        assert!(matches!(
            executor
                .collection(
                    &database,
                    &invalid_plan,
                    &counting_guard,
                    Some(&counting_loader)
                )
                .await,
            Err(SeaOrmExecutionError::InvalidIncludeRelationship {
                resource_type,
                public_name,
            }) if resource_type == expected_resource && public_name == expected_relationship
        ));
    }
    assert_eq!(validation_calls.load(Ordering::SeqCst), 0);
    assert_eq!(loader_calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn executes_sqlite_filters_sort_pagination_fieldsets_and_includes() {
    let database = database().await;
    create_tables(&database).await;
    insert_fixtures(&database).await;

    let foreign_keys = database
        .query_one(sea_orm::Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_keys",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "foreign_keys")
        .unwrap();
    assert_eq!(
        foreign_keys, 1,
        "SQLite connection must enable foreign-key enforcement"
    );

    let invalid_port = port::ActiveModel {
        port_id: Set(99),
        title: Set("Invalid".to_owned()),
        berth_count: Set(None),
        depth_m: Set(0),
        active: Set(false),
        owner_id: Set(Some(999)),
    }
    .insert(&database)
    .await;
    let error = invalid_port.expect_err("SQLite must enforce the owner foreign key");
    assert!(
        error
            .to_string()
            .to_ascii_lowercase()
            .contains("foreign key"),
        "expected an owner foreign-key violation, got {error}"
    );

    let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::new(
        registry(),
        "ports",
        port_resource,
        Arc::new(PortCodec),
    )
    .unwrap();
    let mut unregistered_fieldset = plan(&ReadQuery::default());
    unregistered_fieldset.fieldsets.insert(
        "ports".to_owned(),
        vec![PlannedField::Attribute {
            public_name: "private".to_owned(),
            model_field: "private".to_owned(),
        }],
    );
    let error = executor
        .collection(&database, &unregistered_fieldset, &AllowGuard, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "field `private` is not a valid registered field on resource `ports`"
    );
    let mut mismapped_relationship_fieldset = plan(&ReadQuery::default());
    mismapped_relationship_fieldset.fieldsets.insert(
        "ports".to_owned(),
        vec![PlannedField::Relationship {
            public_name: "owner".to_owned(),
            model_field: "owner_id".to_owned(),
            target_type: "ports".to_owned(),
        }],
    );
    let error = executor
        .collection(
            &database,
            &mismapped_relationship_fieldset,
            &AllowGuard,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "field `owner` is not a valid registered field on resource `ports`"
    );
    let mut unknown_public_sort = plan(&ReadQuery::default());
    unknown_public_sort.sort = vec![SortField {
        public_name: "secret".to_owned(),
        model_field: "depth_m".to_owned(),
        direction: SortDirection::Ascending,
    }];
    let error = executor
        .collection(&database, &unknown_public_sort, &DenyGuard, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sort field `secret` is not enabled for resource `ports`"
    );
    let mut mismapped_sort = plan(&ReadQuery::default());
    mismapped_sort.sort = vec![SortField {
        public_name: "depth".to_owned(),
        model_field: "owner_id".to_owned(),
        direction: SortDirection::Ascending,
    }];
    let error = executor
        .collection(&database, &mismapped_sort, &DenyGuard, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "sort field `depth` is not enabled for resource `ports`"
    );
    for model_field in ["owner_id", "private"] {
        let mut unregistered_filter = plan(&ReadQuery::default());
        unregistered_filter.filter = Some(FilterExpression::Equals {
            model_field: model_field.to_owned(),
            value: FilterValue::String("11".to_owned()),
        });
        let error = executor
            .collection(&database, &unregistered_filter, &DenyGuard, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("filter field `{model_field}` is not enabled for resource `ports`")
        );
    }
    let mut nested_unregistered_filter = plan(&ReadQuery::default());
    nested_unregistered_filter.filter = Some(FilterExpression::And(vec![
        FilterExpression::Equals {
            model_field: "title".to_owned(),
            value: FilterValue::String("Alpha".to_owned()),
        },
        FilterExpression::Not(Box::new(FilterExpression::Equals {
            model_field: "owner_id".to_owned(),
            value: FilterValue::String("11".to_owned()),
        })),
    ]));
    let error = executor
        .collection(&database, &nested_unregistered_filter, &DenyGuard, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "filter field `owner_id` is not enabled for resource `ports`"
    );
    let mut typed_filter = plan(&ReadQuery::default());
    typed_filter.filter = Some(FilterExpression::Equals {
        model_field: "depth_m".to_owned(),
        value: FilterValue::String("2".to_owned()),
    });
    let typed_result = executor
        .collection(&database, &typed_filter, &AllowGuard, None)
        .await
        .unwrap();
    assert_eq!(
        typed_result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        vec!["1"]
    );
    let mut invalid_typed_filter = plan(&ReadQuery::default());
    invalid_typed_filter.filter = Some(FilterExpression::Equals {
        model_field: "berth_count".to_owned(),
        value: FilterValue::String("not-a-number".to_owned()),
    });
    assert!(matches!(
        executor
            .collection(&database, &invalid_typed_filter, &AllowGuard, None)
            .await,
        Err(SeaOrmExecutionError::InvalidFilterValue { model_field, .. })
            if model_field == "berth_count"
    ));

    let query = query_cases::first_page_with_owner();
    let result = executor
        .collection(
            &database,
            &plan(&query),
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();

    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id, query_cases::FIRST_PAGE_PORT_ID);
    assert_eq!(
        result.resources[0].attributes,
        BTreeMap::from([("title".to_owned(), json!(query_cases::FIRST_PAGE_PORT_NAME))])
    );
    assert!(result.resources[0].relationships.contains_key("owner_id"));
    assert_eq!(result.included.len(), 1);
    assert_eq!(result.included[0].resource_type, "people");
    assert_eq!(
        result.included[0].resource.id,
        query_cases::FIRST_PAGE_OWNER_ID
    );
    assert_eq!(
        result.included[0].resource.attributes,
        BTreeMap::from([(
            "display_name".to_owned(),
            json!(query_cases::FIRST_PAGE_OWNER_NAME)
        )])
    );

    let second_page_result = executor
        .collection(
            &database,
            &plan(&query_cases::second_page_without_projection()),
            &AllowGuard,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        second_page_result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        vec![query_cases::SECOND_PAGE_PORT_ID]
    );

    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let limit_calls = Arc::new(AtomicUsize::new(0));
    let bounded_guard = BoundedGuard {
        authorization_calls: authorization_calls.clone(),
        limit_calls: limit_calls.clone(),
        authorized: true,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    let mut maximum_boundary_plan = plan(&ReadQuery::default());
    maximum_boundary_plan.page = Page {
        number: 11,
        size: 10,
        offset: 100,
        limit: 10,
    };
    let maximum_boundary_result = executor
        .collection(&database, &maximum_boundary_plan, &bounded_guard, None)
        .await
        .unwrap();
    assert!(maximum_boundary_result.resources.is_empty());
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 1);
    assert_eq!(limit_calls.load(Ordering::SeqCst), 1);

    for page_number in ["1", "2"] {
        let result = executor
            .collection(
                &database,
                &plan(&query_cases::sorted_ports_page(page_number)),
                &AllowGuard,
                Some(&PortOwnerLoader),
            )
            .await
            .unwrap();
        query_cases::assert_sorted_ports_page(&result, page_number.parse().unwrap());
    }

    for (descending, expected_ids) in [(false, &["1", "2", "3"][..]), (true, &["2", "1", "3"][..])]
    {
        let result = executor
            .collection(
                &database,
                &plan(&query_cases::sorted_nullable_capacity(descending)),
                &AllowGuard,
                None,
            )
            .await
            .unwrap();
        query_cases::assert_sorted_nullable_capacity(&result, expected_ids);
    }

    let multi_field_sorted_result = executor
        .collection(
            &database,
            &plan(&query_cases::multi_field_sorted_ports()),
            &AllowGuard,
            None,
        )
        .await
        .unwrap();
    query_cases::assert_multi_field_sorted_ports(&multi_field_sorted_result);

    let nested_neighbors = executor
        .collection(
            &database,
            &plan(&query_cases::two_level_neighbors()),
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();
    query_cases::assert_two_level_neighbors(&nested_neighbors);

    let unfielded_include_query = ReadQuery {
        includes: vec!["owner".to_owned()],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    };
    let unfielded_result = executor
        .collection(
            &database,
            &plan(&unfielded_include_query),
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();
    let unowned_port = unfielded_result
        .resources
        .iter()
        .find(|resource| resource.id == "3")
        .unwrap();
    assert_eq!(
        unowned_port.relationships["owner_id"].data,
        Some(seamark::document::RelationshipData::Null)
    );
    assert_eq!(
        unfielded_result
            .included
            .iter()
            .map(|included| included.resource.id.as_str())
            .collect::<Vec<_>>(),
        vec!["11", "12"]
    );
    for included in &unfielded_result.included {
        assert_eq!(
            included.resource.attributes,
            query_cases::expected_person_attributes(&included.resource.id)
        );
    }
    let sparse_fieldset_result = executor
        .collection(
            &database,
            &plan(&query_cases::sparse_fieldset_with_owner_include()),
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();
    query_cases::assert_sparse_fieldset_owner_include(&sparse_fieldset_result);

    let neighbors_query = ReadQuery {
        filters: vec!["equals(name,'Alpha')".to_owned()],
        includes: vec!["neighbors".to_owned()],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    };
    let neighbors_result = executor
        .collection(
            &database,
            &plan(&neighbors_query),
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();
    let neighbor_linkage = &neighbors_result.resources[0].relationships["neighbor_ids"].data;
    let neighbor_ids = match neighbor_linkage {
        Some(seamark::document::RelationshipData::Many(identifiers)) => identifiers
            .iter()
            .filter_map(|identifier| identifier.id.as_deref())
            .collect::<Vec<_>>(),
        other => panic!("expected to-many linkage, got {other:?}"),
    };
    assert_eq!(neighbor_ids, query_cases::NEIGHBOR_PORT_IDS);
    let mut included_neighbor_ids = neighbors_result
        .included
        .iter()
        .map(|included| included.resource.id.as_str())
        .collect::<Vec<_>>();
    included_neighbor_ids.sort_unstable();
    assert_eq!(included_neighbor_ids, query_cases::NEIGHBOR_PORT_IDS);

    let single_resource_plan = plan_resource_read(
        &registry(),
        "ports",
        &query_cases::single_resource_with_owner(),
        &pagination(),
    )
    .unwrap();
    let single_resource_result = executor
        .resource(
            &database,
            "1",
            &single_resource_plan,
            &AllowGuard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(single_resource_result.resource.id, "1");
    assert_eq!(
        single_resource_result.resource.attributes,
        BTreeMap::from([("title".to_owned(), json!("Alpha"))])
    );
    assert_eq!(
        single_resource_result.resource.relationships["owner_id"].data,
        Some(seamark::document::RelationshipData::One(
            seamark::document::ResourceIdentifier {
                type_name: "people".to_owned(),
                id: Some("11".to_owned()),
                ..seamark::document::ResourceIdentifier::default()
            }
        ))
    );
    assert_eq!(single_resource_result.included.len(), 1);
    assert_eq!(single_resource_result.included[0].resource_type, "people");
    assert_eq!(single_resource_result.included[0].resource.id, "11");
    assert_eq!(
        single_resource_result.included[0].resource.attributes,
        BTreeMap::from([("display_name".to_owned(), json!("Mara"))])
    );

    for (filter, expected_ids) in query_cases::FILTER_CASES {
        let query = ReadQuery {
            filters: vec![filter.to_owned()],
            page_size: Some("10".to_owned()),
            ..ReadQuery::default()
        };
        let result = executor
            .collection(&database, &plan(&query), &AllowGuard, None)
            .await
            .unwrap();
        assert_eq!(
            result
                .resources
                .iter()
                .map(|resource| resource.id.as_str())
                .collect::<Vec<_>>(),
            expected_ids.to_vec()
        );
    }

    let query_calls = Arc::new(AtomicUsize::new(0));
    let query_adapter = Arc::new(PortHttpQueryAdapter {
        database: database.clone(),
        executor: SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            registry(),
            "ports",
            port_resource as fn(&port::Model) -> AdapterResource,
            PortCodec,
        )
        .unwrap(),
        calls: query_calls.clone(),
    });
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let resource_calls = Arc::new(AtomicUsize::new(0));
    let app = http::router_with_query(
        Arc::new(registry()),
        Arc::new(SingleResourceProbeAdapter {
            calls: resource_calls.clone(),
        }),
        Arc::new(SingleResourceProbeAuthorizer {
            calls: authorization_calls.clone(),
        }),
        query_adapter,
        pagination(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports?filter=equals%28name%2C%27Beta%27%29&sort=-depth&page%5Bsize%5D=1&fields%5Bports%5D=name,owner&fields%5Bpeople%5D=name&include=owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_document: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_document, query_cases::first_page_document());

    let sparse_fieldset_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports?filter=equals%28name%2C%27Alpha%27%29&fields%5Bports%5D=name&fields%5Bpeople%5D=name&include=owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sparse_fieldset_response.status(), StatusCode::OK);
    let sparse_fieldset_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(sparse_fieldset_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        sparse_fieldset_document,
        query_cases::sparse_fieldset_owner_include_document()
    );

    let neighbors_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports?filter=equals%28name%2C%27Alpha%27%29&include=neighbors&fields%5Bports%5D=name,neighbors")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(neighbors_response.status(), StatusCode::OK);
    let neighbors_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(neighbors_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        neighbors_document["data"][0]["relationships"]["neighbors"]["data"],
        json!([
            {"type": "ports", "id": "2"},
            {"type": "ports", "id": "3"}
        ])
    );
    let mut included_neighbors = neighbors_document["included"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    included_neighbors.sort_unstable();
    assert_eq!(included_neighbors, query_cases::NEIGHBOR_PORT_IDS);
    for resource in neighbors_document["included"].as_array().unwrap() {
        let expected_name = match resource["id"].as_str().unwrap() {
            "2" => "Beta",
            "3" => "Gamma",
            id => panic!("unexpected included neighbor `{id}`"),
        };
        assert_eq!(resource["attributes"]["name"], expected_name);
    }

    let resource_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports/1?include=owner&fields%5Bports%5D=name,owner&fields%5Bpeople%5D=name")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resource_response.status(), StatusCode::OK);
    assert_eq!(
        resource_response.headers().get(CONTENT_TYPE).unwrap(),
        "application/vnd.api+json"
    );
    assert_eq!(resource_response.headers().get(VARY).unwrap(), "Accept");
    let resource_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(resource_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        resource_document,
        query_cases::single_resource_owner_document()
    );

    let nested_resource_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports/2?include=neighbors.neighbors&fields%5Bports%5D=name,neighbors")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(nested_resource_response.status(), StatusCode::OK);
    assert_eq!(
        nested_resource_response
            .headers()
            .get(CONTENT_TYPE)
            .unwrap(),
        "application/vnd.api+json"
    );
    assert_eq!(
        nested_resource_response.headers().get(VARY).unwrap(),
        "Accept"
    );
    let nested_resource_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(nested_resource_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    query_cases::assert_single_resource_two_level_neighbors_document(&nested_resource_document);

    let missing_resource_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports/999?include=owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_resource_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing_resource_response
            .headers()
            .get(CONTENT_TYPE)
            .unwrap(),
        "application/vnd.api+json"
    );
    assert_eq!(
        missing_resource_response.headers().get(VARY).unwrap(),
        "Accept"
    );
    let missing_resource_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(missing_resource_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        missing_resource_document,
        query_cases::single_resource_not_found_document("999")
    );

    let invalid_resource_query = app
        .oneshot(
            Request::builder()
                .uri("/ports/1?filter=equals%28name%2C%27Alpha%27%29")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_resource_query.status(), StatusCode::BAD_REQUEST);
    let invalid_resource_document: serde_json::Value = serde_json::from_slice(
        &to_bytes(invalid_resource_query.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        invalid_resource_document["errors"][0]["code"],
        "invalid_query"
    );
    assert_eq!(
        invalid_resource_document["errors"][0]["source"]["parameter"],
        "filter"
    );
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 6);
    assert_eq!(resource_calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_calls.load(Ordering::SeqCst), 6);
    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_single_resource_collection_queries_reject_before_authorization_or_adapters() {
    let database = database().await;
    let resource_calls = Arc::new(AtomicUsize::new(0));
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let query_calls = Arc::new(AtomicUsize::new(0));
    let query_adapter = Arc::new(PortHttpQueryAdapter {
        database: database.clone(),
        executor: SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            registry(),
            "ports",
            port_resource as fn(&port::Model) -> AdapterResource,
            PortCodec,
        )
        .unwrap(),
        calls: query_calls.clone(),
    });
    let app = http::router_with_query(
        Arc::new(registry()),
        Arc::new(SingleResourceProbeAdapter {
            calls: resource_calls.clone(),
        }),
        Arc::new(SingleResourceProbeAuthorizer {
            calls: authorization_calls.clone(),
        }),
        query_adapter,
        pagination(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/ports/1?filter=equals%28name%2C%27Alpha%27%29")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/vnd.api+json"
    );
    assert_eq!(response.headers().get(VARY).unwrap(), "Accept");
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "filter");
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resource_calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn persists_sqlite_atomic_crud_and_relationship_updates() {
    let database = database().await;
    create_tables(&database).await;
    person::ActiveModel {
        person_id: Set(11),
        display_name: Set("Mara".to_owned()),
        private_note: Set("seed".to_owned()),
    }
    .insert(&database)
    .await
    .unwrap();
    let registry = registry();

    let results = execute_sqlite_mutations(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "people", "lid": "person-new", "attributes": {"name": "Rhea", "note": "private"}}},
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "port-new",
                        "attributes": {"name": "New Port", "capacity": 5, "depth": 7, "active": true},
                        "relationships": {"owner": {"data": {"type": "people", "lid": "person-new"}}}
                    }
                },
                {
                    "op": "update",
                    "ref": {"type": "ports", "lid": "port-new"},
                    "data": {"type": "ports", "lid": "port-new", "attributes": {"name": "Renamed Port"}}
                },
                {
                    "op": "update",
                    "ref": {"type": "ports", "lid": "port-new", "relationship": "owner"},
                    "data": {"type": "people", "id": "11"}
                },
                {"op": "remove", "ref": {"type": "ports", "lid": "port-new"}},
                {"op": "remove", "ref": {"type": "people", "lid": "person-new"}}
            ]
        }),
    )
    .await
    .unwrap();

    assert_eq!(results.len(), 6);
    assert_eq!(port::Entity::find().all(&database).await.unwrap().len(), 0);
    assert_eq!(
        person::Entity::find().all(&database).await.unwrap().len(),
        1
    );
    assert_eq!(
        person::Entity::find_by_id(11)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "Mara"
    );

    let app = atomic_http::router(
        Arc::new(registry.clone()),
        database.clone(),
        Arc::new(AllowAtomicGuard),
        Arc::new(mutation_dispatcher(&registry)),
    );
    let successful_request = r#"{"atomic:operations":[{"op":"add","data":{"type":"people","lid":"http-owner","attributes":{"name":"HTTP owner","note":"private"}}},{"op":"add","data":{"type":"ports","lid":"http-port","attributes":{"name":"HTTP port","depth":7,"active":true},"relationships":{"owner":{"data":{"type":"people","lid":"http-owner"}}}}}]}"#;
    let response = app
        .clone()
        .oneshot(atomic_operations_request(successful_request))
        .await
        .unwrap();
    let response_status = response.status();
    let response_content_type = response.headers().get(CONTENT_TYPE).unwrap().clone();
    let response_vary = response.headers().get(VARY).unwrap().clone();
    let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let response_document: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
    assert_eq!(response_status, StatusCode::OK, "{response_document}");
    assert_eq!(response_content_type, ATOMIC_MEDIA_TYPE);
    assert_eq!(response_vary, "Accept");
    assert_eq!(
        response_document["atomic:results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let http_owner_id = response_document["atomic:results"][0]["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let http_port_id = response_document["atomic:results"][1]["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(
        response_document["atomic:results"][0]["data"]["type"],
        "people"
    );
    assert_eq!(
        response_document["atomic:results"][1]["data"]["type"],
        "ports"
    );
    assert_eq!(
        person::Entity::find_by_id(http_owner_id)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .display_name,
        "HTTP owner"
    );
    assert_eq!(
        port::Entity::find_by_id(http_port_id)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .owner_id,
        Some(http_owner_id)
    );
    let failed_request = r#"{"atomic:operations":[{"op":"add","data":{"type":"people","attributes":{"name":"HTTP rollback","note":"private"}}},{"op":"update","ref":{"type":"people","id":"999999"},"data":{"type":"people","attributes":{"name":"missing","note":"private"}}}]}"#;
    let response = app
        .oneshot(atomic_operations_request(failed_request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        ATOMIC_MEDIA_TYPE
    );
    assert_eq!(response.headers().get(VARY).unwrap(), "Accept");
    let error_document: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(error_document["errors"][0]["code"], "operation_failed");
    assert_eq!(
        error_document["errors"][0]["source"]["pointer"],
        "/atomic:operations/1"
    );
    assert!(error_document.get("atomic:results").is_none());
    let people = person::Entity::find().all(&database).await.unwrap();
    assert_eq!(people.len(), 2);
    assert!(
        people
            .iter()
            .all(|person| person.display_name != "HTTP rollback")
    );
    database.close().await.unwrap();
}

struct AllowAtomicGuard;

#[async_trait]
impl AtomicOperationsGuard for AllowAtomicGuard {
    async fn authorize(
        &self,
        _headers: &HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        true
    }

    fn validate_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        Ok(())
    }
}

struct FailSecondWrite {
    calls: AtomicUsize,
}

#[async_trait]
impl AtomicOperationHandler for FailSecondWrite {
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        _operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        transaction
            .execute_unprepared("INSERT INTO seamark_m7_atomic_log (event) VALUES ('write')")
            .await
            .map_err(|error| error.to_string())?;
        if call == 1 {
            return Err("injected operation failure".to_owned());
        }
        Ok(AtomicOperationOutcome::default())
    }
}

#[tokio::test]
async fn rolls_back_sqlite_atomic_writes_after_operation_failure() {
    let database = database().await;
    database
        .execute_unprepared(
            "CREATE TABLE seamark_m7_atomic_log (id INTEGER PRIMARY KEY, event TEXT NOT NULL)",
        )
        .await
        .unwrap();
    let registry = ResourceRegistry::new([
        ResourceDefinition::new("ports", "port_id").attribute("name", "title", false, false)
    ])
    .unwrap();
    let request: AtomicOperationsDocument = serde_json::from_value(json!({
        "atomic:operations": [
            {"op": "update", "ref": {"type": "ports", "id": "1"}, "data": {"type": "ports", "id": "1", "attributes": {"name": "First"}}},
            {"op": "update", "ref": {"type": "ports", "id": "2"}, "data": {"type": "ports", "id": "2", "attributes": {"name": "Second"}}}
        ]
    }))
    .unwrap();
    let operations = plan_atomic_operations(&registry, &request).unwrap();
    let error = execute_atomic_operations(
        &database,
        &operations,
        &HeaderMap::new(),
        &AllowAtomicGuard,
        &FailSecondWrite {
            calls: AtomicUsize::new(0),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        seamark::atomic::AtomicExecutionError::Operation { index: 1, .. }
    ));
    let count = database
        .query_one(sea_orm::Statement::from_string(
            DbBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM seamark_m7_atomic_log",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "count")
        .unwrap();
    assert_eq!(count, 0);

    database.close().await.unwrap();
}

#[tokio::test]
async fn rolls_back_sqlite_typed_mutations_after_a_later_operation_fails() {
    let database = database().await;
    create_tables(&database).await;
    insert_fixtures(&database).await;
    let error = execute_sqlite_mutations(
        &database,
        &registry(),
        json!({
            "atomic:operations": [
                {"op": "update", "ref": {"type": "ports", "id": "1"}, "data": {"type": "ports", "id": "1", "attributes": {"name": "Changed"}}},
                {"op": "update", "ref": {"type": "people", "id": "999"}, "data": {"type": "people", "id": "999", "attributes": {"name": "Missing"}}}
            ]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 1, .. }
    ));
    let port = port::Entity::find_by_id(1)
        .one(&database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.title, "Alpha");

    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_atomic_result_document_matches_shared_backend_case() {
    let database = database().await;
    atomic_cases::create_tables(&database).await;
    atomic_cases::assert_orphan_owner_foreign_key_is_rejected(&database).await;

    let result_document = atomic_cases::execute_case(&database).await;
    assert_eq!(result_document, atomic_cases::expected_result_document());
    atomic_cases::assert_final_state(&database).await;
    let error = atomic_cases::execute_failure_case(&database).await;
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 2, .. }
    ));
    atomic_cases::assert_final_state(&database).await;
    atomic_cases::execute_local_id_to_one_relationship_case(&database).await;
    atomic_cases::execute_to_one_relationship_lifecycle_case(&database).await;
    atomic_cases::execute_to_many_relationship_replacement_case(&database).await;
    atomic_cases::execute_http_to_many_relationship_dispatch_case(&database).await;
    atomic_cases::execute_to_many_foreign_key_relationship_case(&database).await;
    atomic_cases::execute_invalid_result_rollback_case(&database).await;

    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_string_identifiers_and_relationship_mapping_work() {
    let database = database().await;
    string_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_bigint_identifiers_map_to_jsonapi_strings_and_mutate() {
    let database = database().await;
    bigint_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_uuid_identifiers_map_to_canonical_jsonapi_strings_and_mutate() {
    let database = database().await;
    uuid_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_base_http_mutations_preserve_linkage_and_rollback() {
    let database = database().await;
    http_mutation_cases::run_case(&database).await;
    database.close().await.unwrap();
}
