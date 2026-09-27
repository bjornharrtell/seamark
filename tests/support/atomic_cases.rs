use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    QueryOrder, Schema, Set, Value,
};
use seamark::atomic::{
    AtomicExecutionError, AtomicHrefResolver, AtomicOperationHandler, AtomicOperationOutcome,
    AtomicOperationsDocument, AtomicOperationsGuard, AtomicResourceReference, AtomicResult,
    LocalIdMap, PlannedAtomicOperation, PlannedOperation, execute_atomic_operations,
    plan_atomic_operations, plan_atomic_operations_with_href_resolver,
};
use seamark::atomic_http;
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::SeaOrmMutationValueCodec;
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmJoinTableMutationHandler,
    SeaOrmResourceMutationHandler, SeaOrmToManyForeignKeyMutationHandler,
};
use serde_json::{Value as JsonValue, json};
use tower::ServiceExt;

const ATOMIC_MEDIA_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

pub mod person {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_parity_people")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub person_id: i32,
        pub display_name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod port {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_parity_ports")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub port_id: i32,
        pub title: String,
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

pub mod tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_parity_tags")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub tag_id: i32,
        pub tag_name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod port_tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m7_parity_port_tags")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub port_id: i32,
        #[sea_orm(primary_key, auto_increment = false)]
        pub tag_id: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::port::Entity",
            from = "Column::PortId",
            to = "super::port::Column::PortId"
        )]
        Port,
        #[sea_orm(
            belongs_to = "super::tag::Entity",
            from = "Column::TagId",
            to = "super::tag::Column::TagId"
        )]
        Tag,
    }

    impl Related<super::port::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Port.def()
        }
    }

    impl Related<super::tag::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Tag.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

struct MutationCodec;

impl SeaOrmMutationValueCodec for MutationCodec {
    fn encode_mutation_value(&self, field: &str, value: &JsonValue) -> Result<Value, String> {
        match (field, value) {
            ("owner_id", JsonValue::Null) => Ok(Value::Int(None)),
            ("person_id" | "port_id" | "owner_id" | "tag_id", JsonValue::String(value)) => value
                .parse::<i32>()
                .map(|value| Value::Int(Some(value)))
                .map_err(|error| error.to_string()),
            ("display_name" | "title" | "tag_name", JsonValue::String(value)) => {
                Ok(Value::from(value.clone()))
            }
            _ => Err(format!("unsupported value `{value}` for `{field}`")),
        }
    }

    fn decode_identifier(&self, field: &str, value: &Value) -> Result<String, String> {
        match (field, value) {
            ("person_id" | "port_id" | "tag_id", Value::Int(Some(value))) => Ok(value.to_string()),
            _ => Err(format!("unsupported identifier `{value:?}` for `{field}`")),
        }
    }
}

pub struct AllowGuard;

struct InvalidRelationshipResultHandler;
struct MissingClientIdAddResultHandler;

#[async_trait]
impl AtomicOperationsGuard for AllowGuard {
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

#[async_trait]
impl AtomicOperationHandler for InvalidRelationshipResultHandler {
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if !matches!(operation, PlannedOperation::UpdateRelationship { .. }) {
            return Err("expected a relationship update".to_owned());
        }
        transaction
            .execute_unprepared(
                "INSERT INTO seamark_m7_parity_people (person_id, display_name) \
                 VALUES (99, 'must be rolled back')",
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": "people", "id": "99"})),
                meta: None,
            },
            created_resource: None,
        })
    }
}

#[async_trait]
impl AtomicOperationHandler for MissingClientIdAddResultHandler {
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if !matches!(operation, PlannedOperation::AddResource { .. }) {
            return Err("expected a resource add".to_owned());
        }
        transaction
            .execute_unprepared(
                "INSERT INTO seamark_m7_parity_people (person_id, display_name) \
                 VALUES (99, 'must be rolled back')",
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(AtomicOperationOutcome::default())
    }
}

struct ParityHrefResolver;

impl AtomicHrefResolver for ParityHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        let relationship = match href {
            "/ports/1/relationships/owner" => "owner",
            "/ports/1/relationships/tags" => "tags",
            _ => return Ok(None),
        };
        Ok(Some(AtomicResourceReference {
            type_name: "ports".to_owned(),
            id: Some("1".to_owned()),
            lid: None,
            relationship: Some(relationship.to_owned()),
        }))
    }

    fn resolve_resource(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        let id = match href {
            "/ports/1" => "1",
            "/ports/2" => "2",
            _ => return Ok(None),
        };
        Ok(Some(AtomicResourceReference {
            type_name: "ports".to_owned(),
            id: Some(id.to_owned()),
            lid: None,
            relationship: None,
        }))
    }

    fn resolve_collection(&self, href: &str) -> Result<Option<String>, String> {
        Ok((href == "/ports").then(|| "ports".to_owned()))
    }
}

pub fn registry() -> ResourceRegistry {
    ResourceRegistry::new([
        ResourceDefinition::new("people", "person_id")
            .attribute("name", "display_name", false, false)
            .relationship("ports", "owned_ports", "ports"),
        ResourceDefinition::new("ports", "port_id")
            .attribute("name", "title", false, false)
            .relationship("owner", "owner_id", "people")
            .relationship("tags", "tag_links", "tags"),
        ResourceDefinition::new("tags", "tag_id").attribute("name", "tag_name", false, false),
    ])
    .unwrap()
}

pub async fn create_tables(database: &DatabaseConnection) {
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m7_parity_port_tags")
        .await
        .unwrap();
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m7_parity_ports")
        .await
        .unwrap();
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m7_parity_tags")
        .await
        .unwrap();
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m7_parity_people")
        .await
        .unwrap();

    let backend = database.get_database_backend();
    let schema = Schema::new(backend);
    for statement in [
        schema.create_table_from_entity(person::Entity),
        schema.create_table_from_entity(tag::Entity),
        schema.create_table_from_entity(port::Entity),
        schema.create_table_from_entity(port_tag::Entity),
    ] {
        database.execute(backend.build(&statement)).await.unwrap();
    }
}

pub async fn assert_orphan_owner_foreign_key_is_rejected(database: &DatabaseConnection) {
    let error = port::ActiveModel {
        port_id: Set(99),
        title: Set("Orphan".to_owned()),
        owner_id: Set(Some(999)),
    }
    .insert(database)
    .await;
    let Err(error) = error else {
        panic!("the owner foreign key must reject an unknown person ID");
    };
    assert!(
        error
            .to_string()
            .to_ascii_lowercase()
            .contains("foreign key"),
        "expected an owner foreign-key violation, got {error}"
    );
}

pub fn request() -> JsonValue {
    json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "people", "lid": "person-one", "attributes": {"name": "One"}}},
            {"op": "add", "data": {"type": "people", "lid": "person-two", "attributes": {"name": "Two"}}},
            {"op": "add", "data": {"type": "tags", "lid": "tag-one", "attributes": {"name": "Anchor"}}},
            {"op": "add", "data": {"type": "tags", "lid": "tag-two", "attributes": {"name": "Remaining"}}},
            {
                "op": "add",
                "href": "/ports",
                "data": {
                    "type": "ports",
                    "lid": "port-one",
                    "attributes": {"name": "Pier"},
                    "relationships": {"owner": {"data": {"type": "people", "lid": "person-one"}}}
                }
            },
            {
                "op": "add",
                "href": "/ports/1/relationships/tags",
                "data": [
                    {"type": "tags", "lid": "tag-one"},
                    {"type": "tags", "lid": "tag-two"}
                ]
            },
            {
                "op": "remove",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "lid": "tag-one"}]
            },
            {
                "op": "update",
                "href": "/ports/1",
                "data": {"type": "ports", "id": "1", "attributes": {"name": "Updated Pier"}}
            },
            {
                "op": "update",
                "href": "/ports/1/relationships/owner",
                "data": {"type": "people", "lid": "person-two"}
            },
            {"op": "remove", "ref": {"type": "people", "lid": "person-one"}},
            {
                "op": "update",
                "ref": {"type": "tags", "id": "1"},
                "data": {"type": "tags", "id": "1", "attributes": {"name": "Detached"}}
            }
        ]
    })
}

pub fn expected_result_document() -> JsonValue {
    json!({
        "atomic:results": [
            {"data": {"type": "people", "id": "1"}},
            {"data": {"type": "people", "id": "2"}},
            {"data": {"type": "tags", "id": "1"}},
            {"data": {"type": "tags", "id": "2"}},
            {"data": {"type": "ports", "id": "1"}},
            {},
            {},
            {},
            {},
            {},
            {}
        ]
    })
}

pub fn dispatcher(registry: &ResourceRegistry) -> SeaOrmAtomicOperationDispatcher {
    let people = SeaOrmResourceMutationHandler::<person::Entity, _>::new(
        registry,
        "people",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let ports = SeaOrmResourceMutationHandler::<port::Entity, _>::new(
        registry,
        "ports",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let tags = SeaOrmResourceMutationHandler::<tag::Entity, _>::new(
        registry,
        "tags",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let join_table = SeaOrmJoinTableMutationHandler::<port_tag::Entity, _>::new(
        registry,
        "ports",
        "tags",
        "port_id",
        "tag_id",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let owned_ports = SeaOrmToManyForeignKeyMutationHandler::<port::Entity, _>::new(
        registry,
        "people",
        "ports",
        "owner_id",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> = vec![
        Arc::new(owned_ports),
        Arc::new(join_table),
        Arc::new(people),
        Arc::new(ports),
        Arc::new(tags),
    ];
    SeaOrmAtomicOperationDispatcher::new(executors)
}

async fn execute_request(
    database: &DatabaseConnection,
    request: JsonValue,
) -> Result<Vec<AtomicResult>, AtomicExecutionError> {
    let registry = registry();
    let document: AtomicOperationsDocument = serde_json::from_value(request).unwrap();
    let operations =
        plan_atomic_operations_with_href_resolver(&registry, &document, &ParityHrefResolver)
            .unwrap();
    let dispatcher = dispatcher(&registry);
    execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowGuard,
        &dispatcher,
    )
    .await
}

pub async fn execute_http_to_many_relationship_dispatch_case(database: &DatabaseConnection) {
    create_tables(database).await;
    tag::ActiveModel {
        tag_id: Set(1),
        tag_name: Set("First".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    tag::ActiveModel {
        tag_id: Set(2),
        tag_name: Set("Second".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    port::ActiveModel {
        port_id: Set(1),
        title: Set("Pier".to_owned()),
        owner_id: Set(None),
    }
    .insert(database)
    .await
    .unwrap();

    let registry = Arc::new(registry());
    let app = atomic_http::router(
        Arc::clone(&registry),
        database.clone(),
        Arc::new(AllowGuard),
        Arc::new(dispatcher(&registry)),
    );
    let request_body = r#"{"atomic:operations":[{"op":"add","ref":{"type":"ports","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"1"}]},{"op":"add","ref":{"type":"ports","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"2"}]},{"op":"remove","ref":{"type":"ports","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"1"}]}]}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(request_body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_document, json!({"atomic:results": [{}, {}, {}]}));
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!((links[0].port_id, links[0].tag_id), (1, 2));

    let failing_request_body = r#"{"atomic:operations":[{"op":"remove","ref":{"type":"ports","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"2"}]},{"op":"add","ref":{"type":"ports","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"999"}]}]}"#;
    let request = Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(failing_request_body))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(error_document["errors"].as_array().unwrap().len(), 1);
    assert_eq!(error_document["errors"][0]["code"], "operation_failed");
    assert_eq!(error_document["errors"][0]["status"], "422");
    assert_eq!(
        error_document["errors"][0]["title"],
        "Atomic operation failed"
    );
    assert_eq!(
        error_document["errors"][0]["source"]["pointer"],
        "/atomic:operations/1"
    );
    assert!(
        error_document["errors"][0]["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty())
    );
    assert!(error_document.get("atomic:results").is_none());
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!((links[0].port_id, links[0].tag_id), (1, 2));
}

pub async fn execute_http_href_typed_seaorm_case(database: &DatabaseConnection) {
    create_tables(database).await;
    person::ActiveModel {
        person_id: Set(1),
        display_name: Set("Owner".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    for (tag_id, tag_name) in [(1, "First"), (2, "Second")] {
        tag::ActiveModel {
            tag_id: Set(tag_id),
            tag_name: Set(tag_name.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }
    port::ActiveModel {
        port_id: Set(1),
        title: Set("Pier".to_owned()),
        owner_id: Set(None),
    }
    .insert(database)
    .await
    .unwrap();

    let registry = Arc::new(registry());
    let app = atomic_http::router_with_href_resolver(
        Arc::clone(&registry),
        database.clone(),
        Arc::new(AllowGuard),
        Arc::new(dispatcher(&registry)),
        Arc::new(ParityHrefResolver),
    );
    let request_body = json!({
        "atomic:operations": [
            {
                "op": "add",
                "href": "/ports",
                "data": {
                    "type": "ports",
                    "id": "2",
                    "attributes": {"name": "Temporary Pier"}
                }
            },
            {
                "op": "update",
                "href": "/ports/1",
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Updated Pier"}
                }
            },
            {"op": "remove", "href": "/ports/2"},
            {
                "op": "update",
                "href": "/ports/1/relationships/owner",
                "data": {"type": "people", "id": "1"}
            },
            {
                "op": "add",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "id": "1"}, {"type": "tags", "id": "2"}]
            },
            {
                "op": "update",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "id": "2"}]
            },
            {
                "op": "remove",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "id": "2"}]
            }
        ]
    })
    .to_string();
    let response = app
        .clone()
        .oneshot(atomic_request(&request_body))
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(status, StatusCode::OK, "{response_document}");
    assert_eq!(
        response_document,
        json!({"atomic:results": [
            {"data": {"type": "ports", "id": "2"}},
            {},
            {},
            {},
            {},
            {},
            {}
        ]})
    );

    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.title, "Updated Pier");
    assert_eq!(port.owner_id, Some(1));
    assert!(
        port::Entity::find_by_id(2)
            .one(database)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        port_tag::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty()
    );

    let failing_request_body = json!({
        "atomic:operations": [
            {
                "op": "update",
                "href": "/ports/1",
                "data": {
                    "type": "ports",
                    "attributes": {"name": "Must Roll Back"}
                }
            },
            {
                "op": "add",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "id": "1"}]
            },
            {
                "op": "add",
                "href": "/ports/1/relationships/tags",
                "data": [{"type": "tags", "id": "999"}]
            }
        ]
    })
    .to_string();
    let response = app
        .oneshot(atomic_request(&failing_request_body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(error_document["errors"].as_array().unwrap().len(), 1);
    assert_eq!(error_document["errors"][0]["code"], "operation_failed");
    assert_eq!(error_document["errors"][0]["status"], "422");
    assert_eq!(
        error_document["errors"][0]["source"]["pointer"],
        "/atomic:operations/2"
    );
    assert!(error_document.get("atomic:results").is_none());

    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.title, "Updated Pier");
    assert_eq!(port.owner_id, Some(1));
    assert!(
        port_tag::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty(),
        "href-targeted relationship changes preceding a failure must roll back"
    );
}

pub async fn execute_http_href_to_many_relationship_dispatch_case(database: &DatabaseConnection) {
    create_tables(database).await;
    tag::ActiveModel {
        tag_id: Set(1),
        tag_name: Set("First".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    port::ActiveModel {
        port_id: Set(1),
        title: Set("Pier".to_owned()),
        owner_id: Set(None),
    }
    .insert(database)
    .await
    .unwrap();

    let registry = Arc::new(registry());
    let app = atomic_http::router_with_href_resolver(
        Arc::clone(&registry),
        database.clone(),
        Arc::new(AllowGuard),
        Arc::new(dispatcher(&registry)),
        Arc::new(ParityHrefResolver),
    );
    let add_request = Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(
            r#"{"atomic:operations":[{"op":"add","href":"/ports/1/relationships/tags","data":[{"type":"tags","id":"1"}]}]}"#,
        ))
        .unwrap();
    let response = app.clone().oneshot(add_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_document, json!({"atomic:results": [{}]}));
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!((links[0].port_id, links[0].tag_id), (1, 1));

    let remove_request = Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(
            r#"{"atomic:operations":[{"op":"remove","href":"/ports/1/relationships/tags","data":[{"type":"tags","id":"1"}]}]}"#,
        ))
        .unwrap();
    let response = app.oneshot(remove_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_document, json!({"atomic:results": [{}]}));
    assert!(
        port_tag::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn execute_invalid_result_rollback_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let registry = registry();
    let document: AtomicOperationsDocument = serde_json::from_value(json!({
        "atomic:operations": [{
            "op": "update",
            "ref": {"type": "ports", "id": "1", "relationship": "owner"},
            "data": null
        }]
    }))
    .unwrap();
    let operations = plan_atomic_operations(&registry, &document).unwrap();
    let error = execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowGuard,
        &InvalidRelationshipResultHandler,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::InvalidResult { index: 0, .. }
    ));
    assert!(
        person::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty(),
        "writes preceding an invalid server-generated result must roll back"
    );
}

pub async fn execute_client_assigned_add_result_http_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let registry = Arc::new(registry());
    let app = atomic_http::router(
        Arc::clone(&registry),
        database.clone(),
        Arc::new(AllowGuard),
        Arc::new(dispatcher(&registry)),
    );
    let request_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"people","id":"41","attributes":{"name":"Client ID"}}}]}"#;
    let response = app.oneshot(atomic_request(request_body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        response_document,
        json!({"atomic:results": [{"data": {"type": "people", "id": "41"}}]})
    );
    let person = person::Entity::find_by_id(41)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(person.display_name, "Client ID");
}

pub async fn execute_client_assigned_add_missing_result_rollback_http_case(
    database: &DatabaseConnection,
) {
    create_tables(database).await;
    let registry = Arc::new(registry());
    let app = atomic_http::router(
        registry,
        database.clone(),
        Arc::new(AllowGuard),
        Arc::new(MissingClientIdAddResultHandler),
    );
    let request_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"people","id":"99","attributes":{"name":"Client ID"}}}]}"#;
    let response = app.oneshot(atomic_request(request_body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        response_document["errors"][0]["code"],
        "invalid_atomic_response"
    );
    assert_eq!(
        response_document["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );
    assert!(response_document.get("atomic:results").is_none());
    assert!(
        person::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty(),
        "writes preceding an invalid add result must roll back before HTTP responds"
    );
}

fn atomic_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

pub async fn execute_case(database: &DatabaseConnection) -> JsonValue {
    let results = execute_request(database, request()).await.unwrap();
    serde_json::to_value(AtomicOperationsDocument {
        results: Some(results),
        ..AtomicOperationsDocument::default()
    })
    .unwrap()
}

pub async fn execute_local_id_to_one_relationship_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let results = execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "people",
                        "lid": "local-owner",
                        "attributes": {"name": "Local Owner"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "local-port",
                        "attributes": {"name": "Local Port"},
                        "relationships": {
                            "owner": {
                                "data": {"type": "people", "lid": "local-owner"}
                            }
                        }
                    }
                }
            ]
        }),
    )
    .await
    .unwrap();
    let result_document = serde_json::to_value(AtomicOperationsDocument {
        results: Some(results),
        ..AtomicOperationsDocument::default()
    })
    .unwrap();
    assert_eq!(
        result_document,
        json!({
            "atomic:results": [
                {"data": {"type": "people", "id": "1"}},
                {"data": {"type": "ports", "id": "1"}}
            ]
        })
    );

    let people = person::Entity::find().all(database).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].person_id, 1);
    assert_eq!(people[0].display_name, "Local Owner");
    let ports = port::Entity::find().all(database).await.unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].port_id, 1);
    assert_eq!(ports[0].title, "Local Port");
    assert_eq!(ports[0].owner_id, Some(1));
}

pub async fn execute_to_one_relationship_lifecycle_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let create_results = execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "people",
                        "lid": "owner-one",
                        "attributes": {"name": "Owner One"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "people",
                        "lid": "owner-two",
                        "attributes": {"name": "Owner Two"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "owned-port",
                        "attributes": {"name": "Owned Port"},
                        "relationships": {
                            "owner": {"data": {"type": "people", "lid": "owner-one"}}
                        }
                    }
                }
            ]
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(create_results).unwrap(),
        json!([
            {"data": {"type": "people", "id": "1"}},
            {"data": {"type": "people", "id": "2"}},
            {"data": {"type": "ports", "id": "1"}}
        ])
    );
    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.owner_id, Some(1));

    let clear_results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {"type": "ports", "id": "1", "relationship": "owner"},
                "data": null
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(clear_results).unwrap(), json!([{}]));
    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.owner_id, None);

    let reassign_results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {"type": "ports", "id": "1", "relationship": "owner"},
                "data": {"type": "people", "id": "2"}
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(reassign_results).unwrap(), json!([{}]));
    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.owner_id, Some(2));

    let delete_results = execute_request(
        database,
        json!({"atomic:operations": [{"op": "remove", "ref": {"type": "ports", "id": "1"}}]}),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(delete_results).unwrap(), json!([{}]));
    assert!(
        port::Entity::find_by_id(1)
            .one(database)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(person::Entity::find().all(database).await.unwrap().len(), 2);
}

pub async fn execute_to_many_relationship_replacement_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let results = execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "tags",
                        "lid": "first-tag",
                        "attributes": {"name": "First"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "tags",
                        "lid": "second-tag",
                        "attributes": {"name": "Second"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "replacement-port",
                        "attributes": {"name": "Replacement Port"},
                        "relationships": {
                            "tags": {
                                "data": [
                                    {"type": "tags", "lid": "first-tag"},
                                    {"type": "tags", "lid": "second-tag"}
                                ]
                            }
                        }
                    }
                },
                {
                    "op": "update",
                    "href": "/ports/1/relationships/tags",
                    "data": [{"type": "tags", "lid": "second-tag"}]
                },
                {
                    "op": "update",
                    "data": {
                        "type": "ports",
                        "id": "1",
                        "attributes": {"name": "Updated with relationship"},
                        "relationships": {
                            "tags": {
                                "data": [{"type": "tags", "lid": "first-tag"}]
                            }
                        }
                    }
                }
            ]
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        json!([
            {"data": {"type": "tags", "id": "1"}},
            {"data": {"type": "tags", "id": "2"}},
            {"data": {"type": "ports", "id": "1"}},
            {},
            {}
        ])
    );
    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.title, "Updated with relationship");
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].port_id, 1);
    assert_eq!(links[0].tag_id, 1);

    let error = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {
                    "type": "ports",
                    "lid": "rolled-back-port",
                    "attributes": {"name": "Must Roll Back"},
                    "relationships": {
                        "tags": {
                            "data": [{"type": "tags", "id": "999"}]
                        }
                    }
                }
            }]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 0, .. }
    ));
    let ports = port::Entity::find().all(database).await.unwrap();
    assert_eq!(ports.len(), 1);
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].port_id, 1);
    assert_eq!(links[0].tag_id, 1);

    let error = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "data": {
                    "type": "ports",
                    "id": "1",
                    "attributes": {"name": "Must Roll Back"},
                    "relationships": {
                        "tags": {
                            "data": [{"type": "tags", "id": "999"}]
                        }
                    }
                }
            }]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 0, .. }
    ));
    let port = port::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(port.title, "Updated with relationship");
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].port_id, 1);
    assert_eq!(links[0].tag_id, 1);

    let error = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {
                    "type": "ports",
                    "id": "1",
                    "relationship": "tags"
                },
                "data": [
                    {"type": "tags", "id": "1"},
                    {"type": "tags", "id": "999"}
                ]
            }]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 0, .. }
    ));
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].port_id, 1);
    assert_eq!(links[0].tag_id, 1);

    let results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {
                    "type": "ports",
                    "id": "1",
                    "relationship": "tags"
                },
                "data": []
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(results).unwrap(), json!([{}]));
    assert!(
        port_tag::Entity::find()
            .all(database)
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn execute_to_many_foreign_key_relationship_case(database: &DatabaseConnection) {
    create_tables(database).await;
    let results = execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "people",
                        "lid": "owner-one",
                        "attributes": {"name": "Owner One"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "people",
                        "lid": "owner-two",
                        "attributes": {"name": "Owner Two"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "port-one",
                        "attributes": {"name": "One"}
                    }
                },
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "port-two",
                        "attributes": {"name": "Two"}
                    }
                },
                {
                    "op": "add",
                    "ref": {
                        "type": "people",
                        "lid": "owner-one",
                        "relationship": "ports"
                    },
                    "data": [
                        {"type": "ports", "lid": "port-one"},
                        {"type": "ports", "lid": "port-two"}
                    ]
                },
                {
                    "op": "remove",
                    "ref": {
                        "type": "people",
                        "lid": "owner-one",
                        "relationship": "ports"
                    },
                    "data": [{"type": "ports", "lid": "port-one"}]
                }
            ]
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        json!([
            {"data": {"type": "people", "id": "1"}},
            {"data": {"type": "people", "id": "2"}},
            {"data": {"type": "ports", "id": "1"}},
            {"data": {"type": "ports", "id": "2"}},
            {},
            {}
        ])
    );
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, None), (2, Some(1))]
    );

    let results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "data": {
                    "type": "people",
                    "id": "1",
                    "attributes": {"name": "Owner One Updated"},
                    "relationships": {
                        "ports": {
                            "data": [{"type": "ports", "id": "1"}]
                        }
                    }
                }
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(results).unwrap(), json!([{}]));
    let owner = person::Entity::find_by_id(1)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.display_name, "Owner One Updated");
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, Some(1)), (2, None)]
    );

    let results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {
                    "type": "people",
                    "id": "1",
                    "relationship": "ports"
                },
                "data": []
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(results).unwrap(), json!([{}]));
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, None), (2, None)]
    );

    execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "add",
                "ref": {
                    "type": "people",
                    "id": "1",
                    "relationship": "ports"
                },
                "data": [{"type": "ports", "id": "2"}]
            }]
        }),
    )
    .await
    .unwrap();

    let replacement_error = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {
                    "type": "people",
                    "id": "2",
                    "relationship": "ports"
                },
                "data": [
                    {"type": "ports", "id": "1"},
                    {"type": "ports", "id": "2"}
                ]
            }]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        replacement_error,
        AtomicExecutionError::Operation { index: 0, .. }
    ));
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, None), (2, Some(1))]
    );

    let error = execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "ports",
                        "lid": "rolled-back-port",
                        "attributes": {"name": "Must Roll Back"}
                    }
                },
                {
                    "op": "add",
                    "ref": {
                        "type": "people",
                        "id": "2",
                        "relationship": "ports"
                    },
                    "data": [
                        {"type": "ports", "id": "1"},
                        {"type": "ports", "id": "2"}
                    ]
                }
            ]
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicExecutionError::Operation { index: 1, .. }
    ));
    let people = person::Entity::find().all(database).await.unwrap();
    assert_eq!(people.len(), 2);
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, None), (2, Some(1))]
    );

    let results = execute_request(
        database,
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {
                    "type": "people",
                    "attributes": {"name": "Owner Three"},
                    "relationships": {
                        "ports": {
                            "data": [{"type": "ports", "id": "1"}]
                        }
                    }
                }
            }]
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        json!([{"data": {"type": "people", "id": "3"}}])
    );
    let ports = port::Entity::find()
        .order_by_asc(port::Column::PortId)
        .all(database)
        .await
        .unwrap();
    assert_eq!(
        ports
            .iter()
            .map(|port| (port.port_id, port.owner_id))
            .collect::<Vec<_>>(),
        vec![(1, Some(3)), (2, Some(1))]
    );
}

pub async fn execute_failure_case(database: &DatabaseConnection) -> AtomicExecutionError {
    execute_request(
        database,
        json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "data": {
                        "type": "tags",
                        "lid": "rolled-back-tag",
                        "attributes": {"name": "Temporary"}
                    }
                },
                {
                    "op": "add",
                    "href": "/ports/1/relationships/tags",
                    "data": [{"type": "tags", "lid": "rolled-back-tag"}]
                },
                {
                    "op": "update",
                    "ref": {"type": "ports", "id": "999"},
                    "data": {"type": "ports", "id": "999", "attributes": {"name": "Missing"}}
                }
            ]
        }),
    )
    .await
    .unwrap_err()
}

pub async fn assert_final_state(database: &DatabaseConnection) {
    let people = person::Entity::find().all(database).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].person_id, 2);
    assert_eq!(people[0].display_name, "Two");
    let ports = port::Entity::find().all(database).await.unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].port_id, 1);
    assert_eq!(ports[0].title, "Updated Pier");
    assert_eq!(ports[0].owner_id, Some(2));
    let mut tags = tag::Entity::find().all(database).await.unwrap();
    tags.sort_by_key(|tag| tag.tag_id);
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].tag_name, "Detached");
    assert_eq!(tags[1].tag_name, "Remaining");
    let links = port_tag::Entity::find().all(database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].port_id, 1);
    assert_eq!(links[0].tag_id, 2);
}
