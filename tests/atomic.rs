#![allow(missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use seamark::atomic::{
    AtomicExecutionError, AtomicOperationHandler, AtomicOperationOutcome, AtomicOperationsDocument,
    AtomicOperationsError, AtomicOperationsGuard, AtomicResult, LocalIdMap, PlannedAtomicOperation,
    PlannedOperation, execute_atomic_operations, plan_atomic_operations,
};
use seamark::document::{RelationshipData, ResourceIdentifier};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};

fn registry() -> ResourceRegistry {
    let authors =
        ResourceDefinition::new("authors", "author_id").attribute("name", "name", false, false);
    let articles = ResourceDefinition::new("articles", "article_id")
        .attribute("title", "title", false, false)
        .relationship("author", "author_id", "authors")
        .relationship("tags", "tag_ids", "tags");
    let tags = ResourceDefinition::new("tags", "tag_id").attribute("name", "name", false, false);
    ResourceRegistry::new([authors, articles, tags]).unwrap()
}

fn document(value: Value) -> AtomicOperationsDocument {
    serde_json::from_value(value).unwrap()
}

fn plan(value: Value) -> Result<Vec<PlannedAtomicOperation>, AtomicOperationsError> {
    plan_atomic_operations(&registry(), &document(value))
}

#[test]
fn plans_ordered_resource_and_relationship_operations_with_local_ids() {
    let planned = plan(json!({
        "atomic:operations": [
            {
                "op": "add",
                "data": {
                    "type": "authors",
                    "lid": "author-local",
                    "attributes": {"name": "Ada"}
                }
            },
            {
                "op": "add",
                "data": {
                    "type": "articles",
                    "lid": "article-local",
                    "attributes": {"title": "Systems"},
                    "relationships": {
                        "author": {"data": {"type": "authors", "lid": "author-local"}}
                    }
                }
            },
            {
                "op": "update",
                "ref": {"type": "articles", "lid": "article-local"},
                "data": {"type": "articles", "lid": "article-local", "attributes": {"title": "Systems 2"}}
            },
            {
                "op": "update",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "author"},
                "data": null
            },
            {
                "op": "add",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "tags"},
                "data": [{"type": "tags", "id": "tag-1"}]
            },
            {
                "op": "remove",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "tags"},
                "data": [{"type": "tags", "id": "tag-1"}]
            },
            {
                "op": "remove",
                "ref": {"type": "articles", "lid": "article-local"}
            }
        ]
    }))
    .unwrap();
    assert_eq!(planned.len(), 7);
    assert!(matches!(
        planned[0].operation,
        PlannedOperation::AddResource { .. }
    ));
    assert!(matches!(
        planned[3].operation,
        PlannedOperation::UpdateRelationship {
            data: RelationshipData::Null,
            ..
        }
    ));
    assert!(matches!(
        planned[4].operation,
        PlannedOperation::AddRelationshipMembers { .. }
    ));
    assert!(matches!(
        planned[5].operation,
        PlannedOperation::RemoveRelationshipMembers { .. }
    ));
    assert!(matches!(
        planned[6].operation,
        PlannedOperation::RemoveResource { .. }
    ));
}

#[test]
fn accepts_server_assigned_resource_ids_and_preserves_omitted_vs_null_data() {
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "attributes": {"name": "Ada"}}},
            {
                "op": "update",
                "ref": {"type": "articles", "id": "1", "relationship": "author"},
                "data": null
            }
        ]
    }))
    .unwrap();
    let PlannedOperation::AddResource { data, .. } = &operations[0].operation else {
        panic!("expected an add-resource operation");
    };
    assert_eq!(data.id, None);
    assert_eq!(data.lid, None);

    let parsed = document(json!({
        "atomic:operations": [
            {"op": "update", "ref": {"type": "articles", "id": "1", "relationship": "author"}, "data": null},
            {"op": "remove", "ref": {"type": "articles", "id": "1"}}
        ]
    }));
    assert_eq!(
        parsed.operations.as_ref().unwrap()[0].data,
        Some(Value::Null)
    );
    assert_eq!(parsed.operations.as_ref().unwrap()[1].data, None);
}

#[test]
fn accepts_uri_reference_targets_for_resource_mutations() {
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "href": "/author-collection", "data": {"type": "authors", "attributes": {"name": "Ada"}}},
            {"op": "update", "href": "/author-resource/1", "data": {"type": "authors", "id": "1", "attributes": {"name": "Ada Lovelace"}}},
            {"op": "remove", "href": "/author-resource/1"}
        ]
    }))
    .unwrap();
    assert_eq!(operations.len(), 3);

    assert!(
        plan(json!({
            "atomic:operations": [
                {"op": "remove", "href": "not a URI reference"}
            ]
        }))
        .is_err()
    );
}

#[test]
fn validates_request_response_shapes_and_result_cardinality() {
    assert_eq!(
        AtomicOperationsDocument::default().validate_request(),
        Err(AtomicOperationsError::MissingOperations)
    );
    assert_eq!(
        document(json!({"atomic:results": []})).validate_request(),
        Err(AtomicOperationsError::InvalidDocument(
            "an operations request must not contain `atomic:results`"
        ))
    );
    assert_eq!(
        document(json!({"atomic:operations": [], "errors": [{"title": "bad"}]})).validate_request(),
        Err(AtomicOperationsError::InvalidDocument(
            "an operations request must not contain `errors`"
        ))
    );
    assert_eq!(
        document(json!({"atomic:results": [{"data": null}, {}]})).validate_response(1),
        Err(AtomicOperationsError::ResultCountMismatch {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(
        document(json!({"atomic:results": [{}, {}]}))
            .validate_response(2)
            .unwrap()
            .len(),
        2
    );
    assert!(
        serde_json::from_value::<AtomicOperationsDocument>(json!({
            "data": null,
            "atomic:operations": []
        }))
        .is_err()
    );
}

#[test]
fn rejects_malformed_operation_shapes_and_unknown_registry_fields() {
    let cases = [
        json!({"atomic:operations": [{"op": "copy"}]}),
        json!({"atomic:operations": [{"op": "add"}]}),
        json!({"atomic:operations": [{"op": "update", "data": {"type": "authors"}}]}),
        json!({"atomic:operations": [{"op": "remove"}]}),
        json!({"atomic:operations": [{
            "op": "add",
            "ref": {"type": "articles", "id": "1"},
            "data": [{"type": "tags", "id": "2"}]
        }]}),
        json!({"atomic:operations": [{
            "op": "remove",
            "ref": {"type": "articles", "id": "1"},
            "data": null
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "href": "/authors",
            "ref": {"type": "authors", "id": "1"},
            "data": {"type": "authors"}
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "attributes": {"secret": "hidden"}}
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "data": {"type": "articles", "relationships": {
                "author": {"data": {"type": "tags", "id": "2"}}
            }}
        }]}),
    ];
    for value in cases {
        assert!(plan(value).is_err());
    }
}

#[test]
fn rejects_forward_duplicate_and_mismatched_local_id_references() {
    let forward = plan(json!({
        "atomic:operations": [
            {
                "op": "add",
                "data": {"type": "articles", "relationships": {
                    "author": {"data": {"type": "authors", "lid": "future"}}
                }}
            },
            {"op": "add", "data": {"type": "authors", "lid": "future"}}
        ]
    }));
    assert!(matches!(
        forward,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));

    let duplicate = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "same"}},
            {"op": "add", "data": {"type": "authors", "lid": "same"}}
        ]
    }));
    assert!(matches!(
        duplicate,
        Err(AtomicOperationsError::InvalidOperation { index: 1, .. })
    ));

    let mismatch = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author"}},
            {
                "op": "add",
                "data": {"type": "articles", "relationships": {
                    "author": {"data": {"type": "authors", "lid": "not-author"}}
                }}
            }
        ]
    }));
    assert!(mismatch.is_err());
}

#[test]
fn rejects_unassigned_local_ids() {
    let local_ids = LocalIdMap::default();
    let author = ResourceIdentifier {
        type_name: "authors".to_owned(),
        lid: Some("local-1".to_owned()),
        ..ResourceIdentifier::default()
    };
    assert!(local_ids.resolve(&author).is_err());
}

struct TestGuard {
    authorized: bool,
    maximum_operations: usize,
}

#[async_trait]
impl AtomicOperationsGuard for TestGuard {
    async fn authorize(&self, _operations: &[PlannedAtomicOperation]) -> bool {
        self.authorized
    }

    fn validate_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        if operations.len() > self.maximum_operations {
            return Err("operation count limit exceeded".to_owned());
        }
        Ok(())
    }
}

struct LogHandler {
    fail_update: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl AtomicOperationHandler for LogHandler {
    async fn execute_operation(
        &self,
        transaction: &sea_orm::DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match operation {
            PlannedOperation::AddResource { data, .. } => {
                let related_id = data
                    .relationships
                    .as_ref()
                    .and_then(|relationships| relationships.get("author"))
                    .and_then(|relationship| relationship.data.as_ref())
                    .and_then(|relationship_data| match relationship_data {
                        RelationshipData::One(identifier) => Some(identifier),
                        _ => None,
                    })
                    .map(|identifier| local_ids.resolve(identifier))
                    .transpose()?
                    .and_then(|identifier| identifier.id);
                let (event, created_resource) = match data.type_name.as_str() {
                    "authors" => (
                        "author",
                        data.lid.as_ref().map(|_| ResourceIdentifier {
                            type_name: "authors".to_owned(),
                            id: Some("101".to_owned()),
                            ..ResourceIdentifier::default()
                        }),
                    ),
                    "articles" => (
                        "article",
                        data.lid.as_ref().map(|_| ResourceIdentifier {
                            type_name: "articles".to_owned(),
                            id: Some("201".to_owned()),
                            ..ResourceIdentifier::default()
                        }),
                    ),
                    _ => return Err("unexpected resource type".to_owned()),
                };
                transaction
                    .execute(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "INSERT INTO seamark_atomic_log (event, related_id) VALUES ($1, $2)",
                        [event.into(), related_id.into()],
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(AtomicOperationOutcome {
                    result: AtomicResult {
                        data: Some(
                            json!({"type": data.type_name, "id": created_resource.as_ref().and_then(|id| id.id.clone()).unwrap_or_else(|| "202".to_owned())}),
                        ),
                        meta: None,
                    },
                    created_resource,
                })
            }
            PlannedOperation::UpdateResource { .. } => {
                transaction
                    .execute_unprepared("INSERT INTO seamark_atomic_log (event) VALUES ('update')")
                    .await
                    .map_err(|error| error.to_string())?;
                if self.fail_update {
                    Err("injected update failure".to_owned())
                } else {
                    Ok(AtomicOperationOutcome::default())
                }
            }
            _ => Ok(AtomicOperationOutcome::default()),
        }
    }
}

async fn database() -> DatabaseConnection {
    let url = std::env::var("SEAMARK_TEST_DATABASE_URL")
        .expect("set SEAMARK_TEST_DATABASE_URL to a dedicated PostgreSQL test database");
    Database::connect(url).await.unwrap()
}

#[tokio::test]
async fn executes_operations_in_order_maps_local_ids_and_rolls_back_failures() {
    let database = database().await;
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_atomic_log; CREATE TABLE seamark_atomic_log (id BIGSERIAL PRIMARY KEY, event TEXT NOT NULL, related_id TEXT)",
        )
        .await
        .unwrap();
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author-local", "attributes": {"name": "Ada"}}},
            {
                "op": "add",
                "data": {
                    "type": "articles",
                    "lid": "article-local",
                    "relationships": {"author": {"data": {"type": "authors", "lid": "author-local"}}}
                }
            }
        ]
    }))
    .unwrap();
    let handler = LogHandler {
        fail_update: false,
        calls: AtomicUsize::new(0),
    };
    let guard = TestGuard {
        authorized: true,
        maximum_operations: 5,
    };
    let results = execute_atomic_operations(&database, &operations, &guard, &handler)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
    let rows = database
        .query_all(Statement::from_string(
            DbBackend::Postgres,
            "SELECT event, related_id FROM seamark_atomic_log ORDER BY id",
        ))
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<String>("", "event").unwrap(), "author");
    assert_eq!(rows[1].try_get::<String>("", "event").unwrap(), "article");
    assert_eq!(
        rows[1]
            .try_get::<Option<String>>("", "related_id")
            .unwrap()
            .as_deref(),
        Some("101")
    );

    database
        .execute_unprepared("TRUNCATE seamark_atomic_log")
        .await
        .unwrap();
    let failing_operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "attributes": {"name": "First"}}},
            {"op": "update", "ref": {"type": "authors", "id": "10"}, "data": {"type": "authors", "id": "10", "attributes": {"name": "Second"}}}
        ]
    }))
    .unwrap();
    let failing_handler = LogHandler {
        fail_update: true,
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        execute_atomic_operations(&database, &failing_operations, &guard, &failing_handler).await,
        Err(AtomicExecutionError::Operation { index: 1, .. })
    ));
    let count = database
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            "SELECT COUNT(*) AS count FROM seamark_atomic_log",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "count")
        .unwrap();
    assert_eq!(count, 0);

    let denied_guard = TestGuard {
        authorized: false,
        maximum_operations: 5,
    };
    let untouched_handler = LogHandler {
        fail_update: false,
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        execute_atomic_operations(
            &database,
            &failing_operations,
            &denied_guard,
            &untouched_handler
        )
        .await,
        Err(AtomicExecutionError::NotAuthorized)
    ));
    assert_eq!(untouched_handler.calls.load(Ordering::SeqCst), 0);
    let limited_guard = TestGuard {
        authorized: true,
        maximum_operations: 1,
    };
    assert!(matches!(
        execute_atomic_operations(
            &database,
            &failing_operations,
            &limited_guard,
            &untouched_handler
        )
        .await,
        Err(AtomicExecutionError::LimitExceeded(_))
    ));
    assert_eq!(untouched_handler.calls.load(Ordering::SeqCst), 0);

    database
        .execute_unprepared("DROP TABLE seamark_atomic_log")
        .await
        .unwrap();
}
