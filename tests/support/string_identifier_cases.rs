use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Schema, Set, Value,
};
use seamark::atomic::{
    AtomicOperationsDocument, AtomicOperationsGuard, PlannedAtomicOperation,
    execute_atomic_operations, plan_atomic_operations,
};
use seamark::document::{Relationship, RelationshipData, ResourceIdentifier};
use seamark::http::AdapterResource;
use seamark::query::{PaginationConfig, ReadQuery, plan_read, plan_resource_read};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::{
    IncludedResource, SeaOrmFilterValueCodec, SeaOrmIncludeLoader, SeaOrmMutationValueCodec,
    SeaOrmQueryExecutor, SeaOrmReadGuard,
};
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::{Value as JsonValue, json};

pub mod person {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_shared_string_people")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub person_code: String,
        pub display_name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod vessel {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_shared_string_vessels")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub vessel_code: String,
        pub title: String,
        pub owner_code: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::person::Entity",
            from = "Column::OwnerCode",
            to = "super::person::Column::PersonCode"
        )]
        Owner,
    }

    impl Related<super::person::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Owner.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

struct StringCodec;

impl SeaOrmFilterValueCodec for StringCodec {
    fn encode_filter_value(&self, _model_field: &str, value: &str) -> Result<Value, String> {
        Ok(Value::from(value.to_owned()))
    }
}

impl SeaOrmMutationValueCodec for StringCodec {
    fn encode_mutation_value(&self, model_field: &str, value: &JsonValue) -> Result<Value, String> {
        match (model_field, value) {
            ("vessel_code" | "title" | "owner_code", JsonValue::String(value)) => {
                Ok(Value::from(value.clone()))
            }
            _ => Err(format!("unsupported value `{value}` for `{model_field}`")),
        }
    }

    fn decode_identifier(&self, model_field: &str, value: &Value) -> Result<String, String> {
        match (model_field, value) {
            ("vessel_code", Value::String(Some(value))) => Ok(value.to_string()),
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

struct OwnerLoader;

#[async_trait]
impl SeaOrmIncludeLoader<vessel::Entity> for OwnerLoader {
    async fn load_included(
        &self,
        database: &DatabaseConnection,
        roots: &[vessel::Model],
        _includes: &[seamark::query::IncludeNode],
        _fieldsets: &BTreeMap<String, Vec<seamark::query::PlannedField>>,
    ) -> Result<Vec<IncludedResource>, String> {
        let owner_codes = roots
            .iter()
            .map(|root| root.owner_code.clone())
            .collect::<BTreeSet<_>>();
        let owners = person::Entity::find()
            .filter(person::Column::PersonCode.is_in(owner_codes))
            .all(database)
            .await
            .map_err(|error| error.to_string())?;
        Ok(owners
            .into_iter()
            .map(|owner| IncludedResource {
                resource_type: "people".to_owned(),
                resource: AdapterResource {
                    id: owner.person_code,
                    attributes: BTreeMap::from([(
                        "display_name".to_owned(),
                        json!(owner.display_name),
                    )]),
                    ..AdapterResource::default()
                },
            })
            .collect())
    }
}

fn registry() -> ResourceRegistry {
    ResourceRegistry::new([
        ResourceDefinition::new("vessels", "vessel_code")
            .attribute("name", "title", true, true)
            .relationship("owner", "owner_code", "people"),
        ResourceDefinition::new("people", "person_code").attribute(
            "name",
            "display_name",
            false,
            false,
        ),
    ])
    .unwrap()
}

fn vessel_resource(model: &vessel::Model) -> AdapterResource {
    AdapterResource {
        id: model.vessel_code.clone(),
        attributes: BTreeMap::from([("title".to_owned(), json!(model.title))]),
        relationships: BTreeMap::from([(
            "owner_code".to_owned(),
            Relationship {
                data: Some(RelationshipData::One(ResourceIdentifier {
                    type_name: "people".to_owned(),
                    id: Some(model.owner_code.clone()),
                    ..ResourceIdentifier::default()
                })),
                ..Relationship::default()
            },
        )]),
    }
}

pub async fn run(database: &DatabaseConnection) {
    let backend = database.get_database_backend();
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_shared_string_vessels; DROP TABLE IF EXISTS seamark_shared_string_people;",
        )
        .await
        .unwrap();
    let schema = Schema::new(backend);
    for statement in [
        schema.create_table_from_entity(person::Entity),
        schema.create_table_from_entity(vessel::Entity),
    ] {
        database.execute(backend.build(&statement)).await.unwrap();
    }

    for (person_code, display_name) in [("captain-1", "Avery"), ("captain-2", "Blake")] {
        person::ActiveModel {
            person_code: Set(person_code.to_owned()),
            display_name: Set(display_name.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }
    for (vessel_code, title, owner_code) in [
        ("harbor-east", "East Harbor", "captain-1"),
        ("harbor-west", "West Harbor", "captain-2"),
    ] {
        vessel::ActiveModel {
            vessel_code: Set(vessel_code.to_owned()),
            title: Set(title.to_owned()),
            owner_code: Set(owner_code.to_owned()),
        }
        .insert(database)
        .await
        .unwrap();
    }

    let registry = registry();
    let matching_vessels = vessel::Entity::find()
        .filter(vessel::Column::Title.eq("East Harbor"))
        .all(database)
        .await
        .unwrap();
    assert_eq!(matching_vessels.len(), 1);
    let read_query = ReadQuery {
        filters: vec!["equals(name,'East Harbor')".to_owned()],
        sort: Some("name".to_owned()),
        includes: vec!["owner".to_owned()],
        fieldsets: BTreeMap::from([
            ("vessels".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        ..ReadQuery::default()
    };
    let pagination = PaginationConfig::new(1, 10, Some(100), Some(100)).unwrap();
    let plan = plan_read(&registry, "vessels", &read_query, &pagination).unwrap();
    let query_executor = SeaOrmQueryExecutor::<vessel::Entity, _, _>::new(
        registry.clone(),
        "vessels",
        vessel_resource,
        StringCodec,
    )
    .unwrap();
    let query_result = query_executor
        .collection(database, &plan, &AllowReads, Some(&OwnerLoader))
        .await
        .unwrap();
    assert_eq!(query_result.resources.len(), 1);
    assert_eq!(query_result.resources[0].id, "harbor-east");
    assert_eq!(
        query_result.resources[0].attributes["title"],
        json!("East Harbor")
    );
    assert_eq!(query_result.included.len(), 1);
    assert_eq!(query_result.included[0].resource.id, "captain-1");

    let resource_query = ReadQuery {
        includes: vec!["owner".to_owned()],
        fieldsets: BTreeMap::from([
            ("vessels".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        ..ReadQuery::default()
    };
    let resource_plan =
        plan_resource_read(&registry, "vessels", &resource_query, &pagination).unwrap();
    let resource_result = query_executor
        .resource(
            database,
            "harbor-east",
            &resource_plan,
            &AllowReads,
            Some(&OwnerLoader),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resource_result.resource.id, "harbor-east");
    assert_eq!(
        resource_result.resource.attributes,
        BTreeMap::from([("title".to_owned(), json!("East Harbor"))])
    );
    assert_eq!(resource_result.included.len(), 1);
    assert_eq!(resource_result.included[0].resource.id, "captain-1");
    assert_eq!(
        resource_result.included[0].resource.attributes,
        BTreeMap::from([("display_name".to_owned(), json!("Avery"))])
    );

    let document: AtomicOperationsDocument = serde_json::from_value(json!({
        "atomic:operations": [
            {
                "op": "update",
                "ref": {"type": "vessels", "id": "harbor-east"},
                "data": {
                    "type": "vessels",
                    "id": "harbor-east",
                    "attributes": {"name": "East Harbor Updated"}
                }
            },
            {
                "op": "update",
                "ref": {"type": "vessels", "id": "harbor-east", "relationship": "owner"},
                "data": {"type": "people", "id": "captain-2"}
            },
            {
                "op": "add",
                "data": {
                    "type": "vessels",
                    "id": "harbor-north",
                    "attributes": {"name": "North Harbor"},
                    "relationships": {"owner": {"data": {"type": "people", "id": "captain-2"}}}
                }
            },
            {"op": "remove", "ref": {"type": "vessels", "id": "harbor-west"}}
        ]
    }))
    .unwrap();
    let operations = plan_atomic_operations(&registry, &document).unwrap();
    let mutation_handler =
        SeaOrmResourceMutationHandler::<vessel::Entity, _>::new(&registry, "vessels", StringCodec)
            .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> = vec![Arc::new(mutation_handler)];
    let dispatcher = SeaOrmAtomicOperationDispatcher::new(executors);
    let results = execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &AllowOperations,
        &dispatcher,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(results).unwrap(),
        json!([
            {},
            {},
            {"data": {"type": "vessels", "id": "harbor-north"}},
            {}
        ])
    );

    let vessels = vessel::Entity::find().all(database).await.unwrap();
    assert_eq!(vessels.len(), 2);
    assert!(vessels.iter().any(|vessel| {
        vessel.vessel_code == "harbor-east"
            && vessel.title == "East Harbor Updated"
            && vessel.owner_code == "captain-2"
    }));
    assert!(vessels.iter().any(|vessel| {
        vessel.vessel_code == "harbor-north"
            && vessel.title == "North Harbor"
            && vessel.owner_code == "captain-2"
    }));
    assert!(
        person::Entity::find()
            .all(database)
            .await
            .unwrap()
            .iter()
            .any(|person| person.person_code == "captain-2")
    );
}
