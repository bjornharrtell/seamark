use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Schema, Value};
use seamark::atomic::{
    AtomicOperationsDocument, AtomicOperationsGuard, PlannedAtomicOperation,
    execute_atomic_operations, plan_atomic_operations,
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
        pub owner_id: i32,
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

struct MutationCodec;

impl SeaOrmMutationValueCodec for MutationCodec {
    fn encode_mutation_value(&self, field: &str, value: &JsonValue) -> Result<Value, String> {
        match (field, value) {
            ("person_id" | "port_id" | "owner_id", JsonValue::String(value)) => value
                .parse::<i32>()
                .map(|value| Value::Int(Some(value)))
                .map_err(|error| error.to_string()),
            ("display_name" | "title", JsonValue::String(value)) => Ok(Value::from(value.clone())),
            _ => Err(format!("unsupported value `{value}` for `{field}`")),
        }
    }

    fn decode_identifier(&self, field: &str, value: &Value) -> Result<String, String> {
        match (field, value) {
            ("person_id" | "port_id", Value::Int(Some(value))) => Ok(value.to_string()),
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
            .relationship("owner", "owner_id", "people"),
    ])
    .unwrap()
}

pub async fn create_tables(database: &DatabaseConnection) {
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_m7_parity_ports")
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
        schema.create_table_from_entity(port::Entity),
    ] {
        database.execute(backend.build(&statement)).await.unwrap();
    }
}

pub fn request() -> JsonValue {
    json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "people", "lid": "person-one", "attributes": {"name": "One"}}},
            {"op": "add", "data": {"type": "people", "lid": "person-two", "attributes": {"name": "Two"}}},
            {
                "op": "add",
                "data": {
                    "type": "ports",
                    "lid": "port-one",
                    "attributes": {"name": "Pier"},
                    "relationships": {"owner": {"data": {"type": "people", "lid": "person-one"}}}
                }
            },
            {
                "op": "update",
                "ref": {"type": "ports", "lid": "port-one"},
                "data": {"type": "ports", "lid": "port-one", "attributes": {"name": "Updated Pier"}}
            },
            {
                "op": "update",
                "ref": {"type": "ports", "lid": "port-one", "relationship": "owner"},
                "data": {"type": "people", "lid": "person-two"}
            },
            {"op": "remove", "ref": {"type": "ports", "lid": "port-one"}},
            {"op": "remove", "ref": {"type": "people", "lid": "person-one"}}
        ]
    })
}

pub fn expected_result_document() -> JsonValue {
    json!({
        "atomic:results": [
            {"data": {"type": "people", "id": "1"}},
            {"data": {"type": "people", "id": "2"}},
            {"data": {"type": "ports", "id": "1"}},
            {},
            {},
            {},
            {}
        ]
    })
}

pub async fn execute_case(database: &DatabaseConnection) -> JsonValue {
    let registry = registry();
    let document: AtomicOperationsDocument = serde_json::from_value(request()).unwrap();
    let operations = plan_atomic_operations(&registry, &document).unwrap();
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
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> =
        vec![Arc::new(people), Arc::new(ports)];
    let dispatcher = SeaOrmAtomicOperationDispatcher::new(executors);
    let results = execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowGuard,
        &dispatcher,
    )
    .await
    .unwrap();
    serde_json::to_value(AtomicOperationsDocument {
        results: Some(results),
        ..AtomicOperationsDocument::default()
    })
    .unwrap()
}

pub async fn assert_final_state(database: &DatabaseConnection) {
    let people = person::Entity::find().all(database).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].person_id, 2);
    assert_eq!(people[0].display_name, "Two");
    assert!(port::Entity::find().all(database).await.unwrap().is_empty());
}
