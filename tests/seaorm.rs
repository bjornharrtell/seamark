#![allow(missing_docs)]

#[path = "support/query_cases.rs"]
mod query_cases;

#[path = "support/string_identifier_cases.rs"]
mod string_identifier_cases;

#[path = "support/bigint_identifier_cases.rs"]
mod bigint_identifier_cases;

#[path = "support/uuid_identifier_cases.rs"]
mod uuid_identifier_cases;

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
use seamark::query::{
    FilterExpression, FilterValue, IncludeNode, Page, PaginationConfig, PlannedField, ReadPlan,
    ReadQuery, SortDirection, SortField, plan_read,
};
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
            .attribute("capacity", "berth_count", true, true)
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
    loader_calls: Arc<AtomicUsize>,
}

struct UnbackedPortLoader {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SeaOrmIncludeLoader<unbacked_port::Entity> for UnbackedPortLoader {
    async fn load_included(
        &self,
        _database: &DatabaseConnection,
        _roots: &[unbacked_port::Model],
        _includes: &[IncludeNode],
        _fieldsets: &BTreeMap<String, Vec<seamark::query::PlannedField>>,
    ) -> Result<Vec<IncludedResource>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[async_trait]
impl QueryResourceAdapter for UnbackedPortHttpQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let loader = UnbackedPortLoader {
            calls: self.loader_calls.clone(),
        };
        let result = self
            .executor
            .collection(&self.database, plan, &self.guard, Some(&loader))
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
        _headers: &axum::http::HeaderMap,
    ) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        true
    }
}

struct SingleResourceProbeQueryAdapter {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl QueryResourceAdapter for SingleResourceProbeQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        _plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(QueryCollectionResult {
            resources: Vec::new(),
            included: Vec::new(),
        })
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
    loader_calls: Arc<AtomicUsize>,
) -> axum::Router {
    let registry = Arc::new(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "port_key")
                .attribute("name", "title", true, true)
                .relationship("owner", "owner_id", "people"),
            ResourceDefinition::new("people", "person_id"),
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
            loader_calls,
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
    let guard = AllowGuard {
        authorized: true,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    let mut unregistered_fieldset = plan(&ReadQuery::default());
    unregistered_fieldset.fieldsets.insert(
        "ports".to_owned(),
        vec![PlannedField::Attribute {
            public_name: "private".to_owned(),
            model_field: "private".to_owned(),
        }],
    );
    let error = executor
        .collection(&database, &unregistered_fieldset, &guard, None)
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
        .collection(&database, &mismapped_relationship_fieldset, &guard, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "field `owner` is not a valid registered field on resource `ports`"
    );
    let deny_guard = AllowGuard {
        authorized: false,
        maximum_page_size: 10,
        maximum_offset: 100,
    };
    let mut unknown_public_sort = plan(&ReadQuery::default());
    unknown_public_sort.sort = vec![SortField {
        public_name: "secret".to_owned(),
        model_field: "depth_m".to_owned(),
        direction: SortDirection::Ascending,
    }];
    let error = executor
        .collection(&database, &unknown_public_sort, &deny_guard, None)
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
        .collection(&database, &mismapped_sort, &deny_guard, None)
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
            .collection(&database, &unregistered_filter, &deny_guard, None)
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
        .collection(&database, &nested_unregistered_filter, &deny_guard, None)
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
        .collection(&database, &typed_filter, &guard, None)
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
            .collection(&database, &invalid_typed_filter, &guard, None)
            .await,
        Err(SeaOrmExecutionError::InvalidFilterValue { model_field, .. })
            if model_field == "berth_count"
    ));

    let query = query_cases::first_page_with_owner();
    let read_plan = plan(&query);
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

    let mut maximum_boundary_plan = plan(&ReadQuery::default());
    maximum_boundary_plan.page = Page {
        number: 11,
        size: 10,
        offset: 100,
        limit: 10,
    };
    let maximum_boundary_result = executor
        .collection(&database, &maximum_boundary_plan, &guard, None)
        .await
        .unwrap();
    assert!(maximum_boundary_result.resources.is_empty());

    for page_number in ["1", "2"] {
        let result = executor
            .collection(
                &database,
                &plan(&query_cases::sorted_ports_page(page_number)),
                &guard,
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
                &guard,
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
            &guard,
            None,
        )
        .await
        .unwrap();
    query_cases::assert_multi_field_sorted_ports(&multi_field_sorted_result);

    let nested_neighbors = executor
        .collection(
            &database,
            &plan(&query_cases::two_level_neighbors()),
            &guard,
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
            &guard,
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
    assert_query_jsonapi_headers(&sparse_fieldset_response);
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
async fn postgres_single_resource_queries_reject_before_authorization_or_adapters() {
    let resource_calls = Arc::new(AtomicUsize::new(0));
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let query_calls = Arc::new(AtomicUsize::new(0));
    let query_adapter = Arc::new(SingleResourceProbeQueryAdapter {
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
                .uri("/ports/1?include=owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_query_jsonapi_headers(&response);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["errors"][0]["code"], "unsupported_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "include");
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resource_calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn postgres_query_http_rejects_invalid_auth_and_limited_queries_before_sql() {
    let database = database().await;
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m4_unbacked_ports;")
        .await
        .unwrap();
    let schema = Schema::new(DbBackend::Postgres);
    database
        .execute(
            database
                .get_database_backend()
                .build(&schema.create_table_from_entity(unbacked_port::Entity)),
        )
        .await
        .unwrap();
    unbacked_port::ActiveModel {
        port_key: Set(1),
        title: Set("Within limit".to_owned()),
    }
    .insert(&database)
    .await
    .unwrap();

    let invalid_calls = Arc::new(AtomicUsize::new(0));
    let invalid_app = unbacked_query_app(
        database.clone(),
        UnbackedQueryGuard {
            authorized: true,
            maximum_page_size: 10,
        },
        invalid_calls.clone(),
        Arc::new(AtomicUsize::new(0)),
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
        Arc::new(AtomicUsize::new(0)),
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
    let loader_calls = Arc::new(AtomicUsize::new(0));
    let limited_app = unbacked_query_app(
        database.clone(),
        UnbackedQueryGuard {
            authorized: true,
            maximum_page_size: 1,
        },
        limited_calls.clone(),
        loader_calls.clone(),
    );
    let within_limit_response = limited_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ports?include=owner&page%5Bsize%5D=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(within_limit_response.status(), StatusCode::OK);
    assert_query_jsonapi_headers(&within_limit_response);
    let within_limit_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(within_limit_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(within_limit_body["data"][0]["id"], "1");
    assert_eq!(limited_calls.load(Ordering::SeqCst), 1);
    assert_eq!(loader_calls.load(Ordering::SeqCst), 1);

    database
        .execute_unprepared("DROP TABLE seamark_m4_unbacked_ports;")
        .await
        .unwrap();
    let limited_response = limited_app
        .oneshot(
            Request::builder()
                .uri("/ports?include=owner&page%5Bsize%5D=2")
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
    assert_eq!(
        limited_body["errors"],
        json!([{
            "status": "413",
            "code": "resource_limit",
            "title": "Query exceeds configured limits",
            "detail": "The requested query exceeds the server's configured limits."
        }])
    );
    assert_eq!(limited_calls.load(Ordering::SeqCst), 2);
    assert_eq!(loader_calls.load(Ordering::SeqCst), 1);

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

    let guard = AllowGuard {
        authorized: false,
        maximum_page_size: 10,
        maximum_offset: 100,
    };

    let mut denied_plan = plan(&ReadQuery::default());
    denied_plan.sort = vec![seamark::query::SortField {
        public_name: "depth".to_owned(),
        model_field: "depth_m".to_owned(),
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

#[tokio::test]
async fn postgres_bigint_identifiers_map_to_jsonapi_strings_and_mutate() {
    let database = database().await;
    bigint_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}

#[tokio::test]
async fn postgres_uuid_identifiers_map_to_canonical_jsonapi_strings_and_mutate() {
    let database = database().await;
    uuid_identifier_cases::run(&database).await;
    database.close().await.unwrap();
}
