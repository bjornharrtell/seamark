#![allow(missing_docs)]

#[path = "support/query_cases.rs"]
mod query_cases;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend,
    EntityTrait, QueryFilter, Schema, Set, Value,
};
use seamark::document::{Relationship, RelationshipData, ResourceIdentifier};
use seamark::http::{
    self, AdapterError, AdapterIncludedResource, AdapterResource, QueryCollectionResult,
    QueryResourceAdapter, RequestAuthorizer, ResourceAdapter,
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
        pub owner_id: i32,
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

fn resource_definitions() -> (ResourceDefinition, ResourceDefinition) {
    (
        ResourceDefinition::new("ports", "port_id")
            .attribute("name", "title", true, true)
            .attribute("capacity", "berth_count", true, false)
            .attribute("depth", "depth_m", true, true)
            .attribute("active", "active", true, true)
            .relationship("owner", "owner_id", "people"),
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
        relationships: BTreeMap::from([(
            "owner_id".to_owned(),
            Relationship {
                data: Some(RelationshipData::One(ResourceIdentifier {
                    type_name: "people".to_owned(),
                    id: Some(model.owner_id.to_string()),
                    ..ResourceIdentifier::default()
                })),
                ..Relationship::default()
            },
        )]),
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
        if !includes
            .iter()
            .any(|include| include.public_name == "owner")
        {
            return Ok(Vec::new());
        }
        let ids = roots
            .iter()
            .map(|root| root.owner_id)
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
    SeaOrmQueryExecutor<port::Entity, fn(&port::Model) -> AdapterResource, PortFilterCodec>;

struct PortHttpQueryAdapter {
    database: DatabaseConnection,
    executor: PortQueryExecutor,
    guard: AllowGuard,
}

#[async_trait]
impl QueryResourceAdapter for PortHttpQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, AdapterError> {
        let result = self
            .executor
            .collection(&self.database, plan, &self.guard, Some(&PortOwnerLoader))
            .await
            .map_err(|_| AdapterError)?;
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
    assert!(unfielded_result.included.iter().all(|included| {
        included
            .resource
            .attributes
            .keys()
            .all(|field| field == "display_name" || field == "private_note")
    }));

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

    let nested_filter_query = ReadQuery {
        filters: vec!["and(equals(name,'Beta'),not(equals(depth,'2')))".to_owned()],
        ..ReadQuery::default()
    };
    let nested_filter_result = executor
        .collection(&database, &plan(&nested_filter_query), &guard, None)
        .await
        .unwrap();
    assert_eq!(
        nested_filter_result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        vec!["2"]
    );

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

    database
        .execute_unprepared("DROP TABLE seamark_m4_ports; DROP TABLE seamark_m4_people;")
        .await
        .unwrap();
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
