#![allow(missing_docs)]

#[path = "support/query_cases.rs"]
mod query_cases;

#[path = "support/string_identifier_cases.rs"]
mod string_identifier_cases;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, VARY};
use axum::http::{Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend,
    EntityTrait, QueryFilter, Schema, Set, Value,
};
use seamark::document::{Relationship, RelationshipData, ResourceIdentifier};
use seamark::http::{
    self, AdapterError, AdapterIncludedResource, AdapterResource, QueryAdapterError,
    QueryCollectionResult, QueryResourceAdapter, RequestAuthorizer, ResourceAdapter,
};
use seamark::query::{IncludeNode, PaginationConfig, ReadPlan, ReadQuery, plan_read};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::{
    IncludedResource, SeaOrmExecutionError, SeaOrmFilterValueCodec, SeaOrmIncludeLoader,
    SeaOrmQueryExecutor, SeaOrmReadGuard,
};
use serde_json::json;
use tower::ServiceExt;

mod port {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m4_ports")]
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
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod person {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m4_people")]
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

mod unbacked_port {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m4_unbacked_ports")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub port_key: i32,
        pub title: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

fn resource_definitions() -> (ResourceDefinition, ResourceDefinition) {
    (
        ResourceDefinition::new("ports", "port_id")
            .attribute("name", "title", true, true)
            .attribute("capacity", "berth_count", true, false)
            .attribute("depth", "depth_m", true, true)
            .attribute("active", "active", true, true)
            .relationship("owner", "owner_id", "people")
            .relationship("neighbors", "neighbor_ids", "ports"),
        ResourceDefinition::new("people", "person_id")
            .attribute("name", "display_name", false, true)
            .attribute("note", "private_note", false, false),
    )
}

fn registry() -> ResourceRegistry {
    let (ports, people) = resource_definitions();
    ResourceRegistry::new([ports, people]).unwrap()
}

fn pagination() -> PaginationConfig {
    PaginationConfig::new(1, 2, Some(10), Some(100)).unwrap()
}

fn plan(query: &ReadQuery) -> ReadPlan {
    plan_read(&registry(), "ports", query, &pagination()).unwrap()
}

#[test]
fn query_executor_validates_identifier_and_queryable_columns_at_construction() {
    let people = ResourceDefinition::new("people", "person_id");
    let missing_identifier = ResourceRegistry::new([
        ResourceDefinition::new("ports", "missing_id").attribute("name", "title", true, false),
        people.clone(),
    ])
    .unwrap();
    assert!(matches!(
        SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            missing_identifier,
            "ports",
            port_resource,
            PortFilterCodec,
        ),
        Err(SeaOrmExecutionError::UnknownModelField(field)) if field == "missing_id"
    ));

    let missing_query_field = ResourceRegistry::new([
        ResourceDefinition::new("ports", "port_id").attribute(
            "name",
            "missing_column",
            false,
            true,
        ),
        people,
    ])
    .unwrap();
    assert!(matches!(
        SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            missing_query_field,
            "ports",
            port_resource,
            PortFilterCodec,
        ),
        Err(SeaOrmExecutionError::UnknownModelField(field)) if field == "missing_column"
    ));
}

fn port_resource(model: &port::Model) -> AdapterResource {
    AdapterResource {
        id: model.port_id.to_string(),
        attributes: BTreeMap::from([
            ("title".to_owned(), json!(model.title)),
            ("berth_count".to_owned(), json!(model.berth_count)),
            ("depth_m".to_owned(), json!(model.depth_m)),
            ("active".to_owned(), json!(model.active)),
            ("private".to_owned(), json!("must not be mapped")),
        ]),
        relationships: BTreeMap::from([
            (
                "owner_id".to_owned(),
                Relationship {
                    data: Some(match model.owner_id {
                        Some(owner_id) => RelationshipData::One(ResourceIdentifier {
                            type_name: "people".to_owned(),
                            id: Some(owner_id.to_string()),
                            ..ResourceIdentifier::default()
                        }),
                        None => RelationshipData::Null,
                    }),
                    ..Relationship::default()
                },
            ),
            (
                "neighbor_ids".to_owned(),
                Relationship {
                    data: Some(RelationshipData::Many(
                        query_cases::neighbor_ids(model.port_id)
                            .iter()
                            .map(|id| ResourceIdentifier {
                                type_name: "ports".to_owned(),
                                id: Some(id.to_string()),
                                ..ResourceIdentifier::default()
                            })
                            .collect(),
                    )),
                    ..Relationship::default()
                },
            ),
        ]),
    }
}

struct PortFilterCodec;

impl SeaOrmFilterValueCodec for PortFilterCodec {
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

struct AllowGuard {
    authorized: bool,
    maximum_page_size: u64,
    maximum_offset: u64,
}

#[async_trait]
impl SeaOrmReadGuard for AllowGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        self.authorized
    }

    fn validate_limits(&self, plan: &ReadPlan) -> Result<(), String> {
        if plan.page.size > self.maximum_page_size {
            return Err("page size limit exceeded".to_owned());
        }
        if plan.page.offset > self.maximum_offset {
            return Err("offset limit exceeded".to_owned());
        }
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
        if includes
            .iter()
            .any(|include| include.public_name == "neighbors")
        {
            let ids = roots
                .iter()
                .flat_map(|root| query_cases::neighbor_ids(root.port_id).iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let neighbors = port::Entity::find()
                .filter(port::Column::PortId.is_in(ids))
                .all(database)
                .await
                .map_err(|error| error.to_string())?;
            included.extend(neighbors.into_iter().map(|model| IncludedResource {
                resource_type: "ports".to_owned(),
                resource: port_resource(&model),
            }));
        }
        Ok(included)
    }
}

type PortQueryExecutor =
    SeaOrmQueryExecutor<port::Entity, fn(&port::Model) -> AdapterResource, PortFilterCodec>;

struct PortHttpQueryAdapter {
    database: DatabaseConnection,
    executor: PortQueryExecutor,
    guard: AllowGuard,
}

struct UnbackedQueryGuard {
    authorized: bool,
    maximum_page_size: u64,
}

#[async_trait]
impl SeaOrmReadGuard for UnbackedQueryGuard {
    async fn authorize(&self, _plan: &ReadPlan) -> bool {
        self.authorized
    }

    fn validate_limits(&self, plan: &ReadPlan) -> Result<(), String> {
        if plan.page.size > self.maximum_page_size {
            return Err("page size limit exceeded".to_owned());
        }
        Ok(())
    }
}

type UnbackedQueryExecutor = SeaOrmQueryExecutor<
    unbacked_port::Entity,
    fn(&unbacked_port::Model) -> AdapterResource,
    PortFilterCodec,
>;

struct UnbackedPortHttpQueryAdapter {
    database: DatabaseConnection,
    executor: UnbackedQueryExecutor,
    guard: UnbackedQueryGuard,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl QueryResourceAdapter for UnbackedPortHttpQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .executor
            .collection(&self.database, plan, &self.guard, None)
            .await
            .map_err(|error| match error {
                SeaOrmExecutionError::NotAuthorized => QueryAdapterError::NotAuthorized,
                SeaOrmExecutionError::LimitExceeded(_) => QueryAdapterError::LimitExceeded,
                _ => QueryAdapterError::ReadFailed,
            })?;
        Ok(QueryCollectionResult {
            resources: result.resources,
            included: Vec::new(),
        })
    }
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
            .collection(&self.database, plan, &self.guard, Some(&PortOwnerLoader))
            .await
            .map_err(|error| match error {
                SeaOrmExecutionError::NotAuthorized => QueryAdapterError::NotAuthorized,
                SeaOrmExecutionError::LimitExceeded(_) => QueryAdapterError::LimitExceeded,
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
        _headers: &axum::http::HeaderMap,
    ) -> bool {
        true
    }
}

async fn database() -> DatabaseConnection {
    let url = std::env::var("SEAMARK_TEST_DATABASE_URL")
        .expect("set SEAMARK_TEST_DATABASE_URL to a dedicated PostgreSQL test database");
    Database::connect(url).await.unwrap()
}

fn assert_query_jsonapi_headers(response: &axum::response::Response) {
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/vnd.api+json"
    );
    assert_eq!(response.headers().get(VARY).unwrap(), "Accept");
}

fn unbacked_port_resource(model: &unbacked_port::Model) -> AdapterResource {
    AdapterResource {
        id: model.port_key.to_string(),
        attributes: BTreeMap::from([("title".to_owned(), json!(model.title))]),
        ..AdapterResource::default()
    }
}

fn unbacked_query_app(
    database: DatabaseConnection,
    read_guard: UnbackedQueryGuard,
    calls: Arc<AtomicUsize>,
) -> axum::Router {
    let registry = Arc::new(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "port_key").attribute("name", "title", true, true)
        ])
        .unwrap(),
    );
    let executor = SeaOrmQueryExecutor::<unbacked_port::Entity, _, _>::new(
        (*registry).clone(),
        "ports",
        unbacked_port_resource as fn(&unbacked_port::Model) -> AdapterResource,
        PortFilterCodec,
    )
    .unwrap();
    http::router_with_query(
        registry,
        Arc::new(EmptyAdapter),
        Arc::new(AllowHttpRequest),
        Arc::new(UnbackedPortHttpQueryAdapter {
            database,
            executor,
            guard: read_guard,
            calls,
        }),
        pagination(),
    )
}

async fn create_tables(database: &DatabaseConnection) {
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_m4_ports; DROP TABLE IF EXISTS seamark_m4_people;",
        )
        .await
        .unwrap();
    let schema = Schema::new(DbBackend::Postgres);
    database
        .execute(
            database
                .get_database_backend()
                .build(&schema.create_table_from_entity(person::Entity)),
        )
        .await
        .unwrap();
    database
        .execute(
            database
                .get_database_backend()
                .build(&schema.create_table_from_entity(port::Entity)),
        )
        .await
        .unwrap();
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

#[tokio::test]
async fn executes_database_filters_sort_pagination_and_includes_with_fieldsets() {
    let database = database().await;
    create_tables(&database).await;
    insert_fixtures(&database).await;

    let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::new(
        registry(),
        "ports",
        port_resource,
        PortFilterCodec,
    )
    .unwrap();
    let query = query_cases::first_page_with_owner();
    let read_plan = plan(&query);
    let guard = AllowGuard {
        authorized: true,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    let result = executor
        .collection(&database, &read_plan, &guard, Some(&PortOwnerLoader))
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
            &guard,
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
            &guard,
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
        Some(RelationshipData::Null)
    );
    assert_eq!(
        unfielded_result
            .included
            .iter()
            .map(|included| included.resource.id.as_str())
            .collect::<Vec<_>>(),
        vec!["11", "12"]
    );
    assert!(unfielded_result.included.iter().all(|included| {
        included
            .resource
            .attributes
            .keys()
            .all(|field| field == "display_name" || field == "private_note")
    }));

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
            &guard,
            Some(&PortOwnerLoader),
        )
        .await
        .unwrap();
    let neighbor_linkage = &neighbors_result.resources[0].relationships["neighbor_ids"].data;
    let neighbor_ids = match neighbor_linkage {
        Some(RelationshipData::Many(identifiers)) => identifiers
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

    for (filter, expected_ids) in query_cases::FILTER_CASES {
        let query = ReadQuery {
            filters: vec![filter.to_owned()],
            page_size: Some("10".to_owned()),
            ..ReadQuery::default()
        };
        let result = executor
            .collection(&database, &plan(&query), &guard, None)
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

    let http_query_adapter = Arc::new(PortHttpQueryAdapter {
        database: database.clone(),
        executor: SeaOrmQueryExecutor::<port::Entity, _, _>::new(
            registry(),
            "ports",
            port_resource as fn(&port::Model) -> AdapterResource,
            PortFilterCodec,
        )
        .unwrap(),
        guard: AllowGuard {
            authorized: true,
            maximum_page_size: 10,
            maximum_offset: 100,
        },
    });
    let app = http::router_with_query(
        Arc::new(registry()),
        Arc::new(EmptyAdapter),
        Arc::new(AllowHttpRequest),
        http_query_adapter,
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
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body, query_cases::first_page_document());

    let neighbors_response = app
        .oneshot(
            Request::builder()
                .uri("/ports?filter=equals%28name%2C%27Alpha%27%29&include=neighbors&fields%5Bports%5D=name,neighbors")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(neighbors_response.status(), StatusCode::OK);
    assert_query_jsonapi_headers(&neighbors_response);
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

    database
        .execute_unprepared("DROP TABLE seamark_m4_ports; DROP TABLE seamark_m4_people;")
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_query_http_rejects_invalid_auth_and_limited_queries_before_sql() {
    let database = database().await;

    let invalid_calls = Arc::new(AtomicUsize::new(0));
    let invalid_app = unbacked_query_app(
        database.clone(),
        UnbackedQueryGuard {
            authorized: true,
            maximum_page_size: 10,
        },
        invalid_calls.clone(),
    );
    let invalid_response = invalid_app
        .oneshot(
            Request::builder()
                .uri("/ports?filter=equals%28unknown%2C%27x%27%29")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);
    assert_query_jsonapi_headers(&invalid_response);
    let invalid_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(invalid_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(invalid_body["errors"][0]["code"], "invalid_query");
    assert_eq!(invalid_calls.load(Ordering::SeqCst), 0);

    let denied_calls = Arc::new(AtomicUsize::new(0));
    let denied_app = unbacked_query_app(
        database.clone(),
        UnbackedQueryGuard {
            authorized: false,
            maximum_page_size: 10,
        },
        denied_calls.clone(),
    );
    let denied_response = denied_app
        .oneshot(
            Request::builder()
                .uri("/ports?sort=name")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied_response.status(), StatusCode::FORBIDDEN);
    assert_query_jsonapi_headers(&denied_response);
    let denied_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(denied_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(denied_body["errors"][0]["status"], "403");
    assert_eq!(denied_body["errors"][0]["code"], "forbidden");
    assert_eq!(denied_calls.load(Ordering::SeqCst), 1);

    let limited_calls = Arc::new(AtomicUsize::new(0));
    let limited_app = unbacked_query_app(
        database.clone(),
        UnbackedQueryGuard {
            authorized: true,
            maximum_page_size: 1,
        },
        limited_calls.clone(),
    );
    let limited_response = limited_app
        .oneshot(
            Request::builder()
                .uri("/ports?page%5Bsize%5D=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(limited_response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_query_jsonapi_headers(&limited_response);
    let limited_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(limited_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(limited_body["errors"][0]["status"], "413");
    assert_eq!(limited_body["errors"][0]["code"], "resource_limit");
    assert_eq!(limited_calls.load(Ordering::SeqCst), 1);

    database.close().await.unwrap();
}

#[tokio::test]
async fn authorization_limits_and_validation_failures_precede_queries() {
    let database = database().await;
    let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::new(
        registry(),
        "ports",
        port_resource,
        PortFilterCodec,
    )
    .unwrap();
    let guard = AllowGuard {
        authorized: false,
        maximum_page_size: 1,
        maximum_offset: 1,
    };

    let mut denied_plan = plan(&ReadQuery::default());
    denied_plan.sort = vec![seamark::query::SortField {
        public_name: "bad".to_owned(),
        model_field: "not_a_column".to_owned(),
        direction: seamark::query::SortDirection::Ascending,
    }];
    assert!(matches!(
        executor
            .collection(&database, &denied_plan, &guard, None)
            .await,
        Err(SeaOrmExecutionError::NotAuthorized)
    ));

    let allowed = AllowGuard {
        authorized: true,
        maximum_page_size: 1,
        maximum_offset: 1,
    };
    let mut over_limit = plan(&ReadQuery {
        page_size: Some("2".to_owned()),
        ..ReadQuery::default()
    });
    over_limit.sort = denied_plan.sort.clone();
    assert!(matches!(
        executor
            .collection(&database, &over_limit, &allowed, None)
            .await,
        Err(SeaOrmExecutionError::LimitExceeded(_))
    ));

    let mut include_plan = plan(&ReadQuery {
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    });
    include_plan.sort = denied_plan.sort;
    let include_guard = AllowGuard {
        authorized: true,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    assert!(matches!(
        executor
            .collection(&database, &include_plan, &include_guard, None)
            .await,
        Err(SeaOrmExecutionError::IncludeLoaderRequired)
    ));

    let invalid_filter_plan = plan(&ReadQuery {
        filters: vec!["equals(capacity,'not-a-number')".to_owned()],
        ..ReadQuery::default()
    });
    assert!(matches!(
        executor
            .collection(&database, &invalid_filter_plan, &include_guard, None)
            .await,
        Err(SeaOrmExecutionError::InvalidFilterValue { model_field, .. })
            if model_field == "berth_count"
    ));

    let numeric_guard = AllowGuard {
        authorized: true,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    let invalid_numeric_filter = plan(&ReadQuery {
        filters: vec!["equals(capacity,'not-a-number')".to_owned()],
        ..ReadQuery::default()
    });
    assert!(matches!(
        executor
            .collection(&database, &invalid_numeric_filter, &numeric_guard, None)
            .await,
        Err(SeaOrmExecutionError::InvalidFilterValue { .. })
    ));
}

#[tokio::test]
async fn postgres_string_identifiers_and_relationship_mapping_work() {
    let database = database().await;
    string_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}
