#![allow(missing_docs)]

#[path = "support/atomic_cases.rs"]
mod atomic_cases;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, Request, StatusCode};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DatabaseTransaction, DbBackend,
    EntityTrait, QueryFilter, Schema, Set, Value,
};
use seamark::atomic::{
    AtomicExecutionError, AtomicHrefResolver, AtomicOperationOutcome, AtomicOperationsDocument,
    AtomicOperationsGuard, AtomicResourceReference, LocalIdMap, PlannedAtomicOperation,
    PlannedOperation, execute_atomic_operations, plan_atomic_operations,
    plan_atomic_operations_with_href_resolver,
};
use seamark::atomic_http;
use seamark::document::ResourceIdentifier;
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use seamark::seaorm::SeaOrmMutationValueCodec;
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmAtomicOperationExecutor, SeaOrmResourceMutationHandler,
    SeaOrmToManyForeignKeyMutationHandler,
};
use serde_json::{Value as JsonValue, json};
use tower::ServiceExt;

const ATOMIC_MEDIA_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

fn atomic_operations_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/operations")
        .header(CONTENT_TYPE, ATOMIC_MEDIA_TYPE)
        .header(ACCEPT, ATOMIC_MEDIA_TYPE)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

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

mod tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m5_mutation_tags")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub tag_id: i32,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod article_tag {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "seamark_m5_mutation_article_tags")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub article_id: i32,
        #[sea_orm(primary_key, auto_increment = false)]
        pub tag_id: i32,
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

    fn resolve_resource(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if href == format!("/articles/{}", self.article_id) {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some(self.article_id.clone()),
                lid: None,
                relationship: None,
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_collection(&self, href: &str) -> Result<Option<String>, String> {
        Ok((href == "/articles").then(|| "articles".to_owned()))
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
        atomic_cases::enabled_resource("authors", "author_id")
            .mapped_attribute(atomic_cases::enabled_attribute("name", "name"))
            .mapped_relationship(
                atomic_cases::enabled_relationship("articles", "article_links", "articles", false)
                    .to_many_foreign_key(
                        "author_id",
                        true,
                        seamark::registry::RelationshipReassignment::Deny,
                    ),
            ),
        atomic_cases::enabled_resource("articles", "article_id")
            .mapped_attribute(atomic_cases::enabled_attribute("title", "title"))
            .mapped_relationship(atomic_cases::enabled_relationship(
                "author",
                "author_id",
                "authors",
                true,
            ))
            .mapped_relationship(
                atomic_cases::enabled_relationship("tags", "tag_links", "tags", false)
                    .to_many_join_table("article_id", "tag_id"),
            ),
        atomic_cases::enabled_resource("tags", "tag_id")
            .mapped_attribute(atomic_cases::enabled_attribute("name", "name")),
    ])
    .unwrap()
}

struct MutationCodec;

impl SeaOrmMutationValueCodec for MutationCodec {
    fn encode_mutation_value(&self, field: &str, value: &JsonValue) -> Result<Value, String> {
        match (field, value) {
            ("author_id", JsonValue::Null) => Ok(Value::Int(None)),
            ("author_id" | "article_id" | "tag_id", JsonValue::String(value)) => value
                .parse::<i32>()
                .map(|value| Value::Int(Some(value)))
                .map_err(|error| error.to_string()),
            ("name" | "title", JsonValue::String(value)) => Ok(Value::from(value.clone())),
            _ => Err(format!("unsupported value `{value}` for `{field}`")),
        }
    }

    fn decode_identifier(&self, field: &str, value: &Value) -> Result<String, String> {
        match (field, value) {
            ("author_id" | "article_id" | "tag_id", Value::Int(Some(value))) => {
                Ok(value.to_string())
            }
            _ => Err(format!("unsupported identifier `{value:?}` for `{field}`")),
        }
    }
}

struct ArticleTagExecutor;

#[async_trait]
impl SeaOrmAtomicOperationExecutor for ArticleTagExecutor {
    fn supports(&self, operation: &PlannedOperation) -> bool {
        match operation {
            PlannedOperation::AddRelationshipMembers {
                reference,
                model_field,
                ..
            }
            | PlannedOperation::RemoveRelationshipMembers {
                reference,
                model_field,
                ..
            } => reference.type_name == "articles" && model_field == "tag_links",
            _ => false,
        }
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
            _ => return Err("unsupported article-tag operation".to_owned()),
        };
        let article = local_ids.resolve_reference(reference)?;
        let article_id = article
            .id
            .ok_or_else(|| "article target has no persistent identifier".to_owned())?
            .parse::<i32>()
            .map_err(|error| format!("invalid article identifier: {error}"))?;
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
                article_tag::ActiveModel {
                    article_id: Set(article_id),
                    tag_id: Set(tag_id),
                }
                .insert(transaction)
                .await
                .map_err(|error| error.to_string())?;
            }
        } else {
            article_tag::Entity::delete_many()
                .filter(article_tag::Column::ArticleId.eq(article_id))
                .filter(article_tag::Column::TagId.is_in(tag_ids))
                .exec(transaction)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(AtomicOperationOutcome::default())
    }
}

fn dispatcher(registry: &ResourceRegistry) -> SeaOrmAtomicOperationDispatcher {
    let author =
        SeaOrmResourceMutationHandler::<author::Entity, _>::new(registry, "authors", MutationCodec)
            .unwrap();
    let article = SeaOrmResourceMutationHandler::<article::Entity, _>::new(
        registry,
        "articles",
        MutationCodec,
    )
    .unwrap();
    let tag = SeaOrmResourceMutationHandler::<tag::Entity, _>::new(registry, "tags", MutationCodec)
        .unwrap();
    let executors: Vec<Arc<dyn SeaOrmAtomicOperationExecutor>> = vec![
        Arc::new(ArticleTagExecutor),
        Arc::new(author),
        Arc::new(article),
        Arc::new(tag),
    ];
    SeaOrmAtomicOperationDispatcher::new(executors)
}

async fn database() -> Option<DatabaseConnection> {
    let Ok(url) = std::env::var("SEAMARK_TEST_DATABASE_URL") else {
        eprintln!(
            "skipping PostgreSQL test: set SEAMARK_TEST_DATABASE_URL to a dedicated database to run it"
        );
        return None;
    };
    Some(Database::connect(url).await.unwrap())
}

async fn create_tables(database: &DatabaseConnection) {
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_m5_mutation_article_tags; DROP TABLE IF EXISTS seamark_m5_mutation_articles; DROP TABLE IF EXISTS seamark_m5_mutation_authors; DROP TABLE IF EXISTS seamark_m5_mutation_tags;",
        )
        .await
        .unwrap();
    let schema = Schema::new(DbBackend::Postgres);
    for statement in [
        schema.create_table_from_entity(author::Entity),
        schema.create_table_from_entity(article::Entity),
        schema.create_table_from_entity(tag::Entity),
        schema.create_table_from_entity(article_tag::Entity),
    ] {
        database.execute(&statement).await.unwrap();
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

async fn execute_with_href_resolver(
    database: &DatabaseConnection,
    registry: &ResourceRegistry,
    request: JsonValue,
    href_resolver: &dyn AtomicHrefResolver,
) -> Result<Vec<seamark::atomic::AtomicResult>, AtomicExecutionError> {
    let document = document(request);
    let operations =
        plan_atomic_operations_with_href_resolver(registry, &document, href_resolver).unwrap();
    execute_atomic_operations(
        database,
        &operations,
        &HeaderMap::new(),
        &TestGuard,
        &dispatcher(registry),
    )
    .await
}

#[tokio::test]
async fn persists_resource_crud_and_to_one_linkage_atomically() {
    let Some(database) = database().await else {
        return;
    };
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

    let tag_results = execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "tags", "lid": "tag-local", "attributes": {"name": "featured"}}},
                {
                    "op": "add",
                    "ref": {"type": "articles", "id": article_id.to_string(), "relationship": "tags"},
                    "data": [{"type": "tags", "lid": "tag-local"}]
                }
            ]
        }),
    )
    .await
    .unwrap();
    let tag_id = tag_results[0].data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let links = article_tag::Entity::find().all(&database).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].article_id, article_id);
    assert_eq!(links[0].tag_id, tag_id.parse::<i32>().unwrap());

    let failed_to_many = execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "tags", "lid": "rolled-back-tag", "attributes": {"name": "temporary"}}},
                {
                    "op": "add",
                    "ref": {"type": "articles", "id": article_id.to_string(), "relationship": "tags"},
                    "data": [{"type": "tags", "lid": "rolled-back-tag"}]
                },
                {
                    "op": "update",
                    "ref": {"type": "articles", "id": "999999"},
                    "data": {"type": "articles", "id": "999999", "attributes": {"title": "missing"}}
                }
            ]
        }),
    )
    .await;
    assert!(matches!(
        failed_to_many,
        Err(AtomicExecutionError::NotFound { index: 2, .. })
    ));
    assert!(
        tag::Entity::find()
            .all(&database)
            .await
            .unwrap()
            .iter()
            .all(|tag| tag.name != "temporary")
    );
    assert_eq!(
        article_tag::Entity::find()
            .all(&database)
            .await
            .unwrap()
            .len(),
        1
    );

    execute(
        &database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "remove",
                "ref": {"type": "articles", "id": article_id.to_string(), "relationship": "tags"},
                "data": [{"type": "tags", "id": tag_id}]
            }]
        }),
    )
    .await
    .unwrap();
    assert!(
        article_tag::Entity::find()
            .all(&database)
            .await
            .unwrap()
            .is_empty()
    );

    let href_resolver = ArticleAuthorHrefResolver {
        article_id: article_id.to_string(),
    };
    let collection_results = execute_with_href_resolver(
        &database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "add",
                "href": "/articles",
                "data": {
                    "type": "articles",
                    "lid": "href-created",
                    "attributes": {"title": "Created through collection href"}
                }
            }]
        }),
        &href_resolver,
    )
    .await
    .unwrap();
    let href_article_id = collection_results[0].data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resource_href_resolver = ArticleAuthorHrefResolver {
        article_id: href_article_id.clone(),
    };
    execute_with_href_resolver(
        &database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "update",
                "href": format!("/articles/{href_article_id}"),
                "data": {
                    "type": "articles",
                    "attributes": {"title": "Updated through resource href"}
                }
            }]
        }),
        &resource_href_resolver,
    )
    .await
    .unwrap();
    let href_article_pk = href_article_id.parse::<i32>().unwrap();
    assert_eq!(
        article::Entity::find_by_id(href_article_pk)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .title,
        "Updated through resource href"
    );
    execute_with_href_resolver(
        &database,
        &registry,
        json!({
            "atomic:operations": [{
                "op": "remove",
                "href": format!("/articles/{href_article_id}")
            }]
        }),
        &resource_href_resolver,
    )
    .await
    .unwrap();
    assert!(
        article::Entity::find_by_id(href_article_pk)
            .one(&database)
            .await
            .unwrap()
            .is_none()
    );

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
        Err(AtomicExecutionError::NotFound { index: 1, .. })
    ));
    let authors = author::Entity::find().all(&database).await.unwrap();
    assert_eq!(authors.len(), 1);

    let app = atomic_http::router(
        Arc::new(registry.clone()),
        database.clone(),
        Arc::new(TestGuard),
        Arc::new(dispatcher(&registry)),
    );
    let successful_request = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","lid":"http-author","attributes":{"name":"HTTP author"}}},{"op":"add","data":{"type":"articles","lid":"http-article","attributes":{"title":"HTTP article"},"relationships":{"author":{"data":{"type":"authors","lid":"http-author"}}}}}]}"#;
    let response = app
        .clone()
        .oneshot(atomic_operations_request(successful_request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let response_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        response_document["atomic:results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let http_author_id = response_document["atomic:results"][0]["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let http_article_id = response_document["atomic:results"][1]["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert_eq!(
        response_document["atomic:results"][0]["data"]["type"],
        "authors"
    );
    assert_eq!(
        response_document["atomic:results"][1]["data"]["type"],
        "articles"
    );
    assert_eq!(
        author::Entity::find_by_id(http_author_id)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .name,
        "HTTP author"
    );
    assert_eq!(
        article::Entity::find_by_id(http_article_id)
            .one(&database)
            .await
            .unwrap()
            .unwrap()
            .author_id,
        Some(http_author_id)
    );

    let failed_request = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"HTTP rollback"}}},{"op":"update","ref":{"type":"authors","id":"999999"},"data":{"type":"authors","attributes":{"name":"missing"}}}]}"#;
    let response = app
        .oneshot(atomic_operations_request(failed_request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error_document: JsonValue =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(error_document["errors"][0]["code"], "resource_not_found");
    assert_eq!(
        error_document["errors"][0]["source"]["pointer"],
        "/atomic:operations/1"
    );
    assert!(error_document.get("atomic:results").is_none());
    let authors = author::Entity::find().all(&database).await.unwrap();
    assert_eq!(authors.len(), 2);
    assert!(authors.iter().all(|author| author.name != "HTTP rollback"));

    database
        .execute_unprepared(
            "DROP TABLE seamark_m5_mutation_article_tags; DROP TABLE seamark_m5_mutation_articles; DROP TABLE seamark_m5_mutation_authors; DROP TABLE seamark_m5_mutation_tags;",
        )
        .await
        .unwrap();
}

#[test]
fn typed_executor_declines_to_many_relationships_for_application_dispatch() {
    let registry = registry();
    let handler = SeaOrmResourceMutationHandler::<article::Entity, _>::new(
        &registry,
        "articles",
        MutationCodec,
    )
    .unwrap();
    let operations = plan_atomic_operations(
        &registry,
        &document(json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "articles", "lid": "article", "attributes": {"title": "X"}}},
                {"op": "add", "ref": {"type": "articles", "lid": "article", "relationship": "tags"}, "data": [{"type": "tags", "id": "1"}]}
            ]
        })),
    )
    .unwrap();
    assert!(matches!(
        &operations[1].operation,
        PlannedOperation::AddRelationshipMembers { model_field, data, .. }
            if model_field == "tag_links" && data == &[ResourceIdentifier {
                type_name: "tags".to_owned(),
                id: Some("1".to_owned()),
                ..ResourceIdentifier::default()
            }]
    ));
    assert!(!handler.supports(&operations[1].operation));
}

#[test]
fn nullable_foreign_key_executor_supports_add_remove_and_replacement() {
    let registry = registry();
    let handler = SeaOrmToManyForeignKeyMutationHandler::<article::Entity, _>::new(
        &registry,
        "authors",
        "articles",
        MutationCodec,
    )
    .unwrap();
    let operations = plan_atomic_operations(
        &registry,
        &document(json!({
            "atomic:operations": [
                {
                    "op": "add",
                    "ref": {"type": "authors", "id": "1", "relationship": "articles"},
                    "data": [{"type": "articles", "id": "1"}]
                },
                {
                    "op": "remove",
                    "ref": {"type": "authors", "id": "1", "relationship": "articles"},
                    "data": [{"type": "articles", "id": "1"}]
                },
                {
                    "op": "update",
                    "ref": {"type": "authors", "id": "1", "relationship": "articles"},
                    "data": [{"type": "articles", "id": "1"}]
                }
            ]
        })),
    )
    .unwrap();

    assert!(handler.supports(&operations[0].operation));
    assert!(handler.supports(&operations[1].operation));
    assert!(handler.supports(&operations[2].operation));
}

#[test]
fn registry_rejects_reused_join_table_columns() {
    let error = ResourceRegistry::new([
        ResourceDefinition::new("articles", "article_id").mapped_relationship(
            atomic_cases::enabled_relationship("tags", "tag_links", "tags", false)
                .to_many_join_table("article_id", "article_id"),
        ),
        ResourceDefinition::new("tags", "tag_id"),
    ])
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("join-table columns must be non-empty and distinct")
    );
}

#[tokio::test]
async fn postgres_atomic_result_document_matches_shared_backend_case() {
    let Some(database) = database().await else {
        return;
    };
    atomic_cases::create_tables(&database).await;
    atomic_cases::assert_orphan_owner_foreign_key_is_rejected(&database).await;

    let result_document = atomic_cases::execute_case(&database).await;
    assert_eq!(result_document, atomic_cases::expected_result_document());
    atomic_cases::assert_final_state(&database).await;
    let error = atomic_cases::execute_failure_case(&database).await;
    assert!(matches!(
        error,
        AtomicExecutionError::NotFound { index: 2, .. }
    ));
    atomic_cases::assert_final_state(&database).await;
    atomic_cases::execute_local_id_to_one_relationship_case(&database).await;
    atomic_cases::execute_to_one_relationship_lifecycle_case(&database).await;
    atomic_cases::execute_to_many_relationship_replacement_case(&database).await;
    atomic_cases::execute_http_to_many_relationship_dispatch_case(&database).await;
    atomic_cases::execute_http_to_many_foreign_key_idempotent_add_case(&database).await;
    atomic_cases::execute_http_href_typed_seaorm_case(&database).await;
    atomic_cases::execute_http_href_to_many_relationship_dispatch_case(&database).await;
    atomic_cases::execute_to_many_foreign_key_relationship_case(&database).await;
    atomic_cases::execute_invalid_result_rollback_case(&database).await;
    atomic_cases::execute_client_assigned_add_result_http_case(&database).await;
    atomic_cases::execute_duplicate_client_assigned_add_conflict_http_case(&database).await;
    atomic_cases::execute_update_missing_resource_not_found_http_case(&database).await;
    atomic_cases::execute_remove_missing_resource_not_found_http_case(&database).await;
    atomic_cases::execute_client_assigned_add_missing_result_rollback_http_case(&database).await;
    atomic_cases::execute_join_table_insert_columns_case(&database).await;
    atomic_cases::execute_ordered_join_table_case(&database).await;

    database.close().await.unwrap();
}
