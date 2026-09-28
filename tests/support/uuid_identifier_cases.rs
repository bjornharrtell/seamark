use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Schema, Set, Value,
};
use seamark::atomic::{
    AtomicOperationsDocument, AtomicOperationsGuard, PlannedAtomicOperation,
    execute_atomic_operations, plan_atomic_operations,
};
use seamark::http::AdapterResource;
use seamark::query::{PaginationConfig, ReadQuery, plan_read, plan_resource_read};
use seamark::registry::{
    AttributeMapping, AttributePermission, ResourceDefinition, ResourcePermission, ResourceRegistry,
};
use seamark::seaorm::{
    SeaOrmFilterValueCodec, SeaOrmMutationValueCodec, SeaOrmQueryExecutor, SeaOrmReadGuard,
};
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::{Value as JsonValue, json};

pub mod record {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_shared_uuid_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub record_id: Uuid,
        pub lookup_key: Uuid,
        pub label: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const EXISTING_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const EXISTING_LOOKUP_KEY: &str = "9f1c3b4a-5d6e-4789-8abc-0123456789ab";
const OTHER_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
const OTHER_LOOKUP_KEY: &str = "9f1c3b4a-5d6e-4789-8abc-0123456789ac";
const CREATED_ID: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
const CREATED_LOOKUP_KEY: &str = "123e4567-e89b-42d3-a456-426614174000";
const UPDATED_LOOKUP_KEY: &str = "123e4567-e89b-42d3-a456-426614174001";

struct UuidCodec;

fn parse_uuid(value: &str) -> Result<Uuid, String> {
    Uuid::parse_str(value).map_err(|error| error.to_string())
}

impl SeaOrmFilterValueCodec for UuidCodec {
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "lookup_key" => parse_uuid(value).map(|value| Value::Uuid(Some(value))),
            _ => Err(format!(
                "unsupported filter value `{value}` for `{model_field}`"
            )),
        }
    }

    fn encode_resource_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "record_id" => parse_uuid(value).map(|value| Value::Uuid(Some(value))),
            _ => Err(format!(
                "unsupported resource identifier `{value}` for `{model_field}`"
            )),
        }
    }
}

impl SeaOrmMutationValueCodec for UuidCodec {
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        match (model_field, value) {
            ("record_id" | "lookup_key", JsonValue::String(value)) => {
                parse_uuid(value).map(|value| Value::Uuid(Some(value)))
            }
            ("label", JsonValue::String(value)) => Ok(Value::from(value.clone())),
            _ => Err(format!(
                "unsupported mutation value `{value}` for `{model_field}`"
            )),
        }
    }

    fn decode_identifier(&self, model_field: &str, value: &Value) -> Result<String, String> {
        match (model_field, value) {
            ("record_id", Value::Uuid(Some(value))) => Ok(value.to_string()),
            _ => Err(format!(
                "unsupported identifier `{value:?}` for `{model_field}`"
            )),
        }
    }
}

struct AllowReads;

#[async_trait]
impl SeaOrmReadGuard for AllowReads {
    async fn authorize(&self, _plan: &seamark::query::ReadPlan) -> bool {
        true
    }

    fn validate_limits(&self, _plan: &seamark::query::ReadPlan) -> Result<(), String> {
        Ok(())
    }
}

struct AllowOperations;

#[async_trait]
impl AtomicOperationsGuard for AllowOperations {
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

fn registry() -> ResourceRegistry {
    ResourceRegistry::new([ResourceDefinition::new("records", "record_id")
        .allow(ResourcePermission::AtomicCreate)
        .allow(ResourcePermission::AtomicUpdate)
        .allow(ResourcePermission::AtomicDelete)
        .mapped_attribute(
            AttributeMapping::new("lookup_key", "lookup_key")
                .allow(AttributePermission::Filter)
                .allow(AttributePermission::AtomicCreate)
                .allow(AttributePermission::AtomicUpdate),
        )
        .mapped_attribute(
            AttributeMapping::new("label", "label")
                .allow(AttributePermission::AtomicCreate)
                .allow(AttributePermission::AtomicUpdate),
        )])
    .unwrap()
}

fn record_resource(model: &record::Model) -> AdapterResource {
    AdapterResource {
        id: model.record_id.to_string(),
        attributes: BTreeMap::from([
            ("lookup_key".to_owned(), json!(model.lookup_key.to_string())),
            ("label".to_owned(), json!(model.label)),
        ]),
        relationships: BTreeMap::new(),
    }
}

async fn execute_atomic(
    database: &DatabaseConnection,
    registry: &ResourceRegistry,
    request: JsonValue,
) -> Vec<seamark::atomic::AtomicResult> {
    let document: AtomicOperationsDocument = serde_json::from_value(request).unwrap();
    let operations = plan_atomic_operations(registry, &document).unwrap();
    let handler = SeaOrmResourceMutationHandler::<record::Entity, _>::new(
        registry,
        "records",
        Arc::new(UuidCodec),
    )
    .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> = vec![Arc::new(handler)];
    execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowOperations,
        &SeaOrmAtomicOperationDispatcher::new(executors),
    )
    .await
    .unwrap()
}

pub async fn run(database: &DatabaseConnection) {
    let backend = database.get_database_backend();
    database
        .execute_unprepared("DROP TABLE IF EXISTS seamark_shared_uuid_records")
        .await
        .unwrap();
    let schema = Schema::new(backend);
    database
        .execute(&schema.create_table_from_entity(record::Entity))
        .await
        .unwrap();

    for (id, lookup_key, label) in [
        (EXISTING_ID, EXISTING_LOOKUP_KEY, "Existing"),
        (OTHER_ID, OTHER_LOOKUP_KEY, "Other"),
    ] {
        record::ActiveModel {
            record_id: Set(parse_uuid(id).unwrap()),
            lookup_key: Set(parse_uuid(lookup_key).unwrap()),
            label: Set(label.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }

    let registry = registry();
    let query = ReadQuery {
        filters: vec![format!("equals(lookup_key,'{EXISTING_LOOKUP_KEY}')")],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    };
    let pagination = PaginationConfig::new(1, 10, Some(100), Some(100)).unwrap();
    let plan = plan_read(&registry, "records", &query, &pagination).unwrap();
    let executor = SeaOrmQueryExecutor::<record::Entity, _, _>::new(
        registry.clone(),
        "records",
        record_resource,
        UuidCodec,
    )
    .unwrap();
    let result = executor
        .collection(database, &plan, &AllowReads, None)
        .await
        .unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id, EXISTING_ID);
    assert_eq!(
        result.resources[0].attributes,
        BTreeMap::from([
            ("label".to_owned(), json!("Existing")),
            ("lookup_key".to_owned(), json!(EXISTING_LOOKUP_KEY)),
        ])
    );
    let resource_plan =
        plan_resource_read(&registry, "records", &ReadQuery::default(), &pagination).unwrap();
    let resource = executor
        .resource(database, EXISTING_ID, &resource_plan, &AllowReads, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resource.resource.id, EXISTING_ID);
    assert_eq!(
        resource.resource.attributes,
        BTreeMap::from([
            ("label".to_owned(), json!("Existing")),
            ("lookup_key".to_owned(), json!(EXISTING_LOOKUP_KEY)),
        ])
    );

    let create_results = execute_atomic(
        database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {
                    "type": "records",
                    "id": CREATED_ID,
                    "attributes": {
                        "lookup_key": CREATED_LOOKUP_KEY,
                        "label": "Created"
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(
        serde_json::to_value(create_results).unwrap(),
        json!([{"data": {"type": "records", "id": CREATED_ID}}])
    );
    let created_id = parse_uuid(CREATED_ID).unwrap();
    let created = record::Entity::find_by_id(created_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(created.record_id.to_string(), CREATED_ID);
    assert_eq!(created.lookup_key.to_string(), CREATED_LOOKUP_KEY);
    assert_eq!(created.label, "Created");

    let update_results = execute_atomic(
        database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {"type": "records", "id": CREATED_ID},
                "data": {
                    "type": "records",
                    "id": CREATED_ID,
                    "attributes": {
                        "lookup_key": UPDATED_LOOKUP_KEY,
                        "label": "Updated"
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(serde_json::to_value(update_results).unwrap(), json!([{}]));
    let updated = record::Entity::find_by_id(created_id)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.record_id.to_string(), CREATED_ID);
    assert_eq!(updated.lookup_key.to_string(), UPDATED_LOOKUP_KEY);
    assert_eq!(updated.label, "Updated");

    let delete_results = execute_atomic(
        database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "remove",
                "ref": {"type": "records", "id": CREATED_ID}
            }]
        }),
    )
    .await;
    assert_eq!(serde_json::to_value(delete_results).unwrap(), json!([{}]));
    assert!(
        record::Entity::find_by_id(created_id)
            .one(database)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        record::Entity::find_by_id(parse_uuid(EXISTING_ID).unwrap())
            .one(database)
            .await
            .unwrap()
            .is_some()
    );
}
