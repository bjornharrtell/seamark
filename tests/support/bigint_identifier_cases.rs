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
    #[sea_orm(table_name = "seamark_shared_bigint_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub record_id: i64,
        pub score: i64,
        pub label: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const EXISTING_ID: i64 = 9_007_199_254_740_993;
const CREATED_ID: i64 = i64::MAX;
const SEARCH_SCORE: i64 = 5_000_000_002;

struct BigIntCodec;

impl SeaOrmFilterValueCodec for BigIntCodec {
    fn encode_filter_value(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "score" => value
                .parse::<i64>()
                .map(|value| Value::BigInt(Some(value)))
                .map_err(|error| error.to_string()),
            _ => Err(format!(
                "unsupported filter value `{value}` for `{model_field}`"
            )),
        }
    }

    fn encode_resource_identifier(&self, model_field: &str, value: &str) -> Result<Value, String> {
        match model_field {
            "record_id" => value
                .parse::<i64>()
                .map(|value| Value::BigInt(Some(value)))
                .map_err(|error| error.to_string()),
            _ => Err(format!(
                "unsupported resource identifier `{value}` for `{model_field}`"
            )),
        }
    }
}

impl SeaOrmMutationValueCodec for BigIntCodec {
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        match (model_field, value) {
            ("record_id" | "score", JsonValue::String(value)) => value
                .parse::<i64>()
                .map(|value| Value::BigInt(Some(value)))
                .map_err(|error| error.to_string()),
            ("score", JsonValue::Number(value)) => value
                .as_i64()
                .map(|value| Value::BigInt(Some(value)))
                .ok_or_else(|| format!("invalid integer value `{value}` for `{model_field}`")),
            ("label", JsonValue::String(value)) => Ok(Value::from(value.clone())),
            _ => Err(format!(
                "unsupported mutation value `{value}` for `{model_field}`"
            )),
        }
    }

    fn decode_identifier(&self, model_field: &str, value: &Value) -> Result<String, String> {
        match (model_field, value) {
            ("record_id", Value::BigInt(Some(value))) => Ok(value.to_string()),
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
            AttributeMapping::new("score", "score")
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
            ("score".to_owned(), json!(model.score)),
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
        Arc::new(BigIntCodec),
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
        .execute_unprepared("DROP TABLE IF EXISTS seamark_shared_bigint_records")
        .await
        .unwrap();
    let schema = Schema::new(backend);
    database
        .execute(&schema.create_table_from_entity(record::Entity))
        .await
        .unwrap();

    record::ActiveModel {
        record_id: Set(EXISTING_ID),
        score: Set(SEARCH_SCORE),
        label: Set("Existing".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();
    record::ActiveModel {
        record_id: Set(EXISTING_ID + 1),
        score: Set(SEARCH_SCORE + 1),
        label: Set("Other".to_owned()),
    }
    .insert(database)
    .await
    .unwrap();

    let registry = registry();
    let query = ReadQuery {
        filters: vec![format!("equals(score,'{SEARCH_SCORE}')")],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    };
    let pagination = PaginationConfig::new(1, 10, Some(100), Some(100)).unwrap();
    let plan = plan_read(&registry, "records", &query, &pagination).unwrap();
    let executor = SeaOrmQueryExecutor::<record::Entity, _, _>::new(
        registry.clone(),
        "records",
        record_resource,
        BigIntCodec,
    )
    .unwrap();
    let result = executor
        .collection(database, &plan, &AllowReads, None)
        .await
        .unwrap();
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id, EXISTING_ID.to_string());
    assert_eq!(
        result.resources[0].attributes,
        BTreeMap::from([
            ("label".to_owned(), json!("Existing")),
            ("score".to_owned(), json!(SEARCH_SCORE)),
        ])
    );
    let resource_plan =
        plan_resource_read(&registry, "records", &ReadQuery::default(), &pagination).unwrap();
    let resource = executor
        .resource(
            database,
            &EXISTING_ID.to_string(),
            &resource_plan,
            &AllowReads,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resource.resource.id, EXISTING_ID.to_string());
    assert_eq!(
        resource.resource.attributes,
        BTreeMap::from([
            ("label".to_owned(), json!("Existing")),
            ("score".to_owned(), json!(SEARCH_SCORE)),
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
                    "id": CREATED_ID.to_string(),
                    "attributes": {"label": "Created", "score": SEARCH_SCORE + 3}
                }
            }]
        }),
    )
    .await;
    assert_eq!(
        serde_json::to_value(create_results).unwrap(),
        json!([{"data": {"type": "records", "id": CREATED_ID.to_string()}}])
    );
    assert_eq!(
        record::Entity::find_by_id(CREATED_ID)
            .one(database)
            .await
            .unwrap()
            .unwrap()
            .label,
        "Created"
    );

    let update_results = execute_atomic(
        database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "update",
                "ref": {"type": "records", "id": CREATED_ID.to_string()},
                "data": {
                    "type": "records",
                    "id": CREATED_ID.to_string(),
                    "attributes": {"label": "Updated", "score": SEARCH_SCORE + 4}
                }
            }]
        }),
    )
    .await;
    assert_eq!(serde_json::to_value(update_results).unwrap(), json!([{}]));
    let updated = record::Entity::find_by_id(CREATED_ID)
        .one(database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.label, "Updated");
    assert_eq!(updated.score, SEARCH_SCORE + 4);

    let delete_results = execute_atomic(
        database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "remove",
                "ref": {"type": "records", "id": CREATED_ID.to_string()}
            }]
        }),
    )
    .await;
    assert_eq!(serde_json::to_value(delete_results).unwrap(), json!([{}]));
    assert!(
        record::Entity::find_by_id(CREATED_ID)
            .one(database)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        record::Entity::find_by_id(EXISTING_ID)
            .one(database)
            .await
            .unwrap()
            .unwrap()
            .label,
        "Existing"
    );
}
