#![cfg(feature = "sqlite")]
#![allow(missing_docs)]

#[path = "support/query_cases.rs"]
mod query_cases;

#[path = "support/atomic_cases.rs"]
mod atomic_cases;

#[path = "support/string_identifier_cases.rs"]
mod string_identifier_cases;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
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
use seamark::http::{
    self, AdapterError, AdapterIncludedResource, AdapterResource, QueryAdapterError,
    QueryCollectionResult, QueryResourceAdapter, RequestAuthorizer, ResourceAdapter,
};
use seamark::query::{IncludeNode, PaginationConfig, ReadPlan, ReadQuery, plan_read};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::{
    IncludedResource, SeaOrmFilterValueCodec, SeaOrmIncludeLoader, SeaOrmMutationValueCodec,
    SeaOrmQueryExecutor, SeaOrmReadGuard,
};
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::json;
use tower::ServiceExt;

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
            .attribute("capacity", "berth_count", true, false)
            .attribute("depth", "depth_m", true, true)
            .attribute("active", "active", true, true)
            .relationship("owner", "owner_id", "people"),
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
        relationships: BTreeMap::from([(
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
        )]),
    }
}

struct PortCodec;

impl SeaOrmFilterValueCodec for PortCodec {
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "berth_count" | "depth_m" => value
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
        if !includes
            .iter()
            .any(|include| include.public_name == "owner")
        {
            return Ok(Vec::new());
        }

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
        Ok(people
            .into_iter()
            .map(|model| IncludedResource {
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
            })
            .collect())
    }
}

type PortQueryExecutor =
    SeaOrmQueryExecutor<port::Entity, fn(&port::Model) -> AdapterResource, PortCodec>;

struct PortHttpQueryAdapter {
    database: DatabaseConnection,
    executor: PortQueryExecutor,
}

#[async_trait]
impl QueryResourceAdapter for PortHttpQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
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
}

#[derive(Default)]
struct EmptyAdapter;

#[async_trait]
impl ResourceAdapter for EmptyAdapter {
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
        Ok(None)
    }
}

struct AllowHttpRequest;

#[async_trait]
impl RequestAuthorizer for AllowHttpRequest {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &HeaderMap,
    ) -> bool {
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
async fn executes_sqlite_filters_sort_pagination_fieldsets_and_includes() {
    let database = database().await;
    create_tables(&database).await;
    insert_fixtures(&database).await;

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
    assert!(
        invalid_port.is_err(),
        "SQLite must enforce the owner foreign key"
    );

    let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::new(
        registry(),
        "ports",
        port_resource,
        Arc::new(PortCodec),
    )
    .unwrap();
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

    let query_adapter = Arc::new(PortHttpQueryAdapter {
        database: database.clone(),
        executor: SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            registry(),
            "ports",
            port_resource as fn(&port::Model) -> AdapterResource,
            PortCodec,
        )
        .unwrap(),
    });
    let app = http::router_with_query(
        Arc::new(registry()),
        Arc::new(EmptyAdapter),
        Arc::new(AllowHttpRequest),
        query_adapter,
        pagination(),
    );
    let response = app
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

    let result_document = atomic_cases::execute_case(&database).await;
    assert_eq!(result_document, atomic_cases::expected_result_document());
    atomic_cases::assert_final_state(&database).await;
    let error = atomic_cases::execute_failure_case(&database).await;
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 1, .. }
    ));
    atomic_cases::assert_final_state(&database).await;

    database.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_string_identifiers_and_relationship_mapping_work() {
    let database = database().await;
    string_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}
