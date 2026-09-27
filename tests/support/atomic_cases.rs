use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, Schema, Set, Value,
};
use seamark::atomic::{
    AtomicExecutionError, AtomicHrefResolver, AtomicOperationOutcome, AtomicOperationsDocument,
    AtomicOperationsGuard, AtomicResourceReference, AtomicResult, LocalIdMap,
    PlannedAtomicOperation, PlannedOperation, execute_atomic_operations,
    plan_atomic_operations_with_href_resolver,
};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::SeaOrmMutationValueCodec;
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::{Value as JsonValue, json};

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

struct AllowGuard;

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

struct PortTagExecutor;

#[async_trait]
impl SeaOrmAtomicOperationExecutor for PortTagExecutor {
    fn supports(&self, operation: &PlannedOperation) -> bool {
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
            } if reference.type_name == "ports" && model_field == "tag_links"
        )
    }

    async fn execute(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let (reference, identifiers, add) = match operation {
            PlannedOperation::AddRelationshipMembers {
                reference, data, ..
            } => (reference, data, true),
            PlannedOperation::RemoveRelationshipMembers {
                reference, data, ..
            } => (reference, data, false),
            _ => return Err("unsupported port-tag operation".to_owned()),
        };
        let port = local_ids.resolve_reference(reference)?;
        let port_id = port
            .id
            .ok_or_else(|| "port target has no persistent identifier".to_owned())?
            .parse::<i32>()
            .map_err(|error| format!("invalid port identifier: {error}"))?;
        let tag_ids = identifiers
            .iter()
            .map(|identifier| {
                local_ids
                    .resolve(identifier)?
                    .id
                    .ok_or_else(|| "tag target has no persistent identifier".to_owned())?
                    .parse::<i32>()
                    .map_err(|error| format!("invalid tag identifier: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        if add {
            for tag_id in tag_ids {
                port_tag::ActiveModel {
                    port_id: Set(port_id),
                    tag_id: Set(tag_id),
                }
                .insert(transaction)
                .await
                .map_err(|error| error.to_string())?;
            }
        } else {
            port_tag::Entity::delete_many()
                .filter(port_tag::Column::PortId.eq(port_id))
                .filter(port_tag::Column::TagId.is_in(tag_ids))
                .exec(transaction)
                .await
                .map_err(|error| error.to_string())?;
        }
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
        Ok((href == "/ports/1").then(|| AtomicResourceReference {
            type_name: "ports".to_owned(),
            id: Some("1".to_owned()),
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
        ResourceDefinition::new("people", "person_id").attribute(
            "name",
            "display_name",
            false,
            false,
        ),
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

async fn execute_request(
    database: &DatabaseConnection,
    request: JsonValue,
) -> Result<Vec<AtomicResult>, AtomicExecutionError> {
    let registry = registry();
    let document: AtomicOperationsDocument = serde_json::from_value(request).unwrap();
    let operations =
        plan_atomic_operations_with_href_resolver(&registry, &document, &ParityHrefResolver)
            .unwrap();
    let people = SeaOrmResourceMutationHandler::<person::Entity, _>::new(
        &registry,
        "people",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let ports = SeaOrmResourceMutationHandler::<port::Entity, _>::new(
        &registry,
        "ports",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let tags = SeaOrmResourceMutationHandler::<tag::Entity, _>::new(
        &registry,
        "tags",
        Arc::new(MutationCodec),
    )
    .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> = vec![
        Arc::new(PortTagExecutor),
        Arc::new(people),
        Arc::new(ports),
        Arc::new(tags),
    ];
    let dispatcher = SeaOrmAtomicOperationDispatcher::new(executors);
    execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowGuard,
        &dispatcher,
    )
    .await
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
