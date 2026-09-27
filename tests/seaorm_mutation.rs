#![allow(missing_docs)]

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait, Schema, Value,
};
use seamark::atomic::{
    AtomicExecutionError, AtomicHrefResolver, AtomicOperationsDocument, AtomicOperationsGuard,
    AtomicResourceReference, PlannedAtomicOperation, PlannedOperation, execute_atomic_operations,
    plan_atomic_operations, plan_atomic_operations_with_href_resolver,
};
use seamark::document::ResourceIdentifier;
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
};
use serde_json::{Value as JsonValue, json};

mod author {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m5_mutation_authors")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub author_id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod article {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m5_mutation_articles")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub article_id: i32,
        pub title: String,
        pub author_id: Option<i32>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

struct TestGuard;

struct ArticleAuthorHrefResolver {
    article_id: String,
}

impl AtomicHrefResolver for ArticleAuthorHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if href == format!("/articles/{}/relationships/author", self.article_id) {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some(self.article_id.clone()),
                relationship: Some("author".to_owned()),
                lid: None,
            }))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl AtomicOperationsGuard for TestGuard {
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
    ResourceRegistry::new([
        ResourceDefinition::new("authors", "author_id").attribute("name", "name", false, false),
        ResourceDefinition::new("articles", "article_id")
            .attribute("title", "title", false, false)
            .relationship("author", "author_id", "authors"),
    ])
    .unwrap()
}

fn encode_value(field: &str, value: &JsonValue) -> Result<Value, String> {
    match (field, value) {
        ("author_id", JsonValue::Null) => Ok(Value::Int(None)),
        ("author_id" | "article_id", JsonValue::String(value)) => value
            .parse::<i32>()
            .map(|value| Value::Int(Some(value)))
            .map_err(|error| error.to_string()),
        ("name" | "title", JsonValue::String(value)) => Ok(Value::from(value.clone())),
        _ => Err(format!("unsupported value `{value}` for `{field}`")),
    }
}

fn decode_identifier(field: &str, value: &Value) -> Result<String, String> {
    match (field, value) {
        ("author_id" | "article_id", Value::Int(Some(value))) => Ok(value.to_string()),
        _ => Err(format!("unsupported identifier `{value:?}` for `{field}`")),
    }
}

fn dispatcher(registry: &ResourceRegistry) -> SeaOrmAtomicOperationDispatcher {
    let author = SeaOrmResourceMutationHandler::<author::Entity, _, _>::new(
        registry,
        "authors",
        encode_value,
        decode_identifier,
    )
    .unwrap();
    let article = SeaOrmResourceMutationHandler::<article::Entity, _, _>::new(
        registry,
        "articles",
        encode_value,
        decode_identifier,
    )
    .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> =
        vec![Arc::new(author), Arc::new(article)];
    SeaOrmAtomicOperationDispatcher::new(executors)
}

async fn database() -> DatabaseConnection {
    let url = std::env::var("SEAMARK_TEST_DATABASE_URL")
        .expect("set SEAMARK_TEST_DATABASE_URL to a dedicated PostgreSQL test database");
    Database::connect(url).await.unwrap()
}

async fn create_tables(database: &DatabaseConnection) {
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_m5_mutation_articles; DROP TABLE IF EXISTS seamark_m5_mutation_authors;",
        )
        .await
        .unwrap();
    let schema = Schema::new(DbBackend::Postgres);
    for statement in [
        schema.create_table_from_entity(author::Entity),
        schema.create_table_from_entity(article::Entity),
    ] {
        database
            .execute(database.get_database_backend().build(&statement))
            .await
            .unwrap();
    }
}

fn document(value: JsonValue) -> AtomicOperationsDocument {
    serde_json::from_value(value).unwrap()
}

async fn execute(
    database: &DatabaseConnection,
    registry: &ResourceRegistry,
    request: JsonValue,
) -> Result<Vec<seamark::atomic::AtomicResult>, AtomicExecutionError> {
    let document = document(request);
    let operations = plan_atomic_operations(registry, &document).unwrap();
    let headers = HeaderMap::new();
    execute_atomic_operations(
        database,
        &operations,
        &headers,
        &TestGuard,
        &dispatcher(registry),
    )
    .await
}

#[tokio::test]
async fn persists_resource_crud_and_to_one_linkage_atomically() {
    let database = database().await;
    create_tables(&database).await;
    let registry = registry();

    let results = execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "authors", "lid": "author-1", "attributes": {"name": "Ada"}}},
                {
                    "op": "add",
                    "data": {
                        "type": "articles",
                        "lid": "article-1",
                        "attributes": {"title": "Engine"},
                        "relationships": {"author": {"data": {"type": "authors", "lid": "author-1"}}}
                    }
                }
            ]
        }),
    )
    .await
    .unwrap();
    assert_eq!(results.len(), 2);
    let author_id = results[0].data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let article_id = results[1].data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let article = article::Entity::find_by_id(article_id)
        .one(&database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(article.title, "Engine");
    assert_eq!(article.author_id, Some(author_id));

    execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {
                    "op": "update",
                    "ref": {"type": "articles", "id": article_id.to_string(), "relationship": "author"},
                    "data": null
                },
                {
                    "op": "update",
                    "ref": {"type": "articles", "id": article_id.to_string()},
                    "data": {"type": "articles", "id": article_id.to_string(), "attributes": {"title": "Rebuilt"}}
                }
            ]
        }),
    )
    .await
    .unwrap();
    let article = article::Entity::find_by_id(article_id)
        .one(&database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(article.title, "Rebuilt");
    assert_eq!(article.author_id, None);

    let href_resolver = ArticleAuthorHrefResolver {
        article_id: article_id.to_string(),
    };
    let href_document = document(json!({
        "atomic:operations": [{
            "op": "update",
            "href": format!("/articles/{article_id}/relationships/author"),
            "data": {"type": "authors", "id": author_id.to_string()}
        }]
    }));
    let href_operations =
        plan_atomic_operations_with_href_resolver(&registry, &href_document, &href_resolver)
            .unwrap();
    assert!(matches!(
        &href_operations[0].operation,
        PlannedOperation::UpdateRelationship { model_field, .. } if model_field == "author_id"
    ));
    execute_atomic_operations(
        &database,
        &href_operations,
        &HeaderMap::new(),
        &TestGuard,
        &dispatcher(&registry),
    )
    .await
    .unwrap();
    let article = article::Entity::find_by_id(article_id)
        .one(&database)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(article.author_id, Some(author_id));

    execute(
        &database,
        &registry,
        json!({"atomic:operations":[{"op":"remove","ref":{"type":"articles","id":article_id.to_string()}}]}),
    )
    .await
    .unwrap();
    assert!(
        article::Entity::find_by_id(article_id)
            .one(&database)
            .await
            .unwrap()
            .is_none()
    );

    let existing_authors = author::Entity::find().all(&database).await.unwrap();
    assert_eq!(existing_authors.len(), 1);
    assert_eq!(existing_authors[0].name, "Ada");

    let failing = execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "authors", "attributes": {"name": "Grace"}}},
                {"op": "update", "ref": {"type": "authors", "id": "999999"}, "data": {"type": "authors", "id": "999999", "attributes": {"name": "Missing"}}}
            ]
        }),
    )
    .await;
    assert!(matches!(
        failing,
        Err(AtomicExecutionError::Operation { index: 1, .. })
    ));
    let authors = author::Entity::find().all(&database).await.unwrap();
    assert_eq!(authors.len(), 1);

    database
        .execute_unprepared(
            "DROP TABLE seamark_m5_mutation_articles; DROP TABLE seamark_m5_mutation_authors;",
        )
        .await
        .unwrap();
}

#[test]
fn typed_executor_declines_to_many_relationships_for_application_dispatch() {
    let registry = registry();
    let handler = SeaOrmResourceMutationHandler::<article::Entity, _, _>::new(
        &registry,
        "articles",
        encode_value,
        decode_identifier,
    )
    .unwrap();
    let operations = plan_atomic_operations(
        &registry,
        &document(json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "articles", "lid": "article", "attributes": {"title": "X"}}},
                {"op": "add", "ref": {"type": "articles", "lid": "article", "relationship": "author"}, "data": [{"type": "authors", "id": "1"}]}
            ]
        })),
    )
    .unwrap();
    assert!(matches!(
        &operations[1].operation,
        PlannedOperation::AddRelationshipMembers { model_field, data, .. }
            if model_field == "author_id" && data == &[ResourceIdentifier {
                type_name: "authors".to_owned(),
                id: Some("1".to_owned()),
                ..ResourceIdentifier::default()
            }]
    ));
    assert!(!handler.supports(&operations[1].operation));
}
