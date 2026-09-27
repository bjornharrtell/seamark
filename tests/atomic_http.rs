#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{Request, Response, StatusCode};
use sea_orm::{Database, DatabaseConnection, DatabaseTransaction};
use seamark::atomic::{
    AtomicHrefResolver, AtomicOperationHandler, AtomicOperationOutcome, AtomicOperationsGuard,
    AtomicResourceReference, AtomicResult, LocalIdMap, PlannedAtomicOperation, PlannedOperation,
};
use seamark::atomic_http;
use seamark::document::ResourceIdentifier;
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};
use tower::ServiceExt;

const ATOMIC_MEDIA_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

struct TestGuard {
    allowed: bool,
}

struct CountingGuard {
    calls: AtomicUsize,
}

#[async_trait]
impl AtomicOperationsGuard for TestGuard {
    async fn authorize(
        &self,
        _headers: &axum::http::HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        self.allowed
    }

    fn validate_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        Ok(())
    }
}

#[async_trait]
impl AtomicOperationsGuard for CountingGuard {
    async fn authorize(
        &self,
        _headers: &axum::http::HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn validate_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        Ok(())
    }
}

struct LimitedGuard;

#[async_trait]
impl AtomicOperationsGuard for LimitedGuard {
    async fn authorize(
        &self,
        _headers: &axum::http::HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        true
    }

    fn validate_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        Err("operation limit exceeded".to_owned())
    }
}

struct TestHandler {
    fail: bool,
    require_resolved_targets: bool,
}

struct AtMemberHandler {
    calls: AtomicUsize,
}

struct FailSecondHandler {
    calls: AtomicUsize,
}

struct CountingHandler {
    calls: AtomicUsize,
}

struct LocalIdHandler {
    calls: AtomicUsize,
}

#[async_trait]
impl AtomicOperationHandler for AtMemberHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        let PlannedOperation::AddResource { changeset, .. } = operation else {
            return Err("expected an add-resource operation".to_owned());
        };
        let Some(attributes) = changeset.attributes.as_ref() else {
            return Err("expected mapped attributes".to_owned());
        };
        if attributes.len() != 1 || attributes.get("name") != Some(&json!("Ada")) {
            return Err("unexpected mapped attributes".to_owned());
        }
        if changeset.relationships.is_some() {
            return Err("unexpected mapped relationships".to_owned());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": "authors", "id": "created"})),
                meta: None,
            },
            created_resource: None,
        })
    }
}

#[async_trait]
impl AtomicOperationHandler for FailSecondHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        _operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err("injected second-operation failure".to_owned());
        }
        Ok(AtomicOperationOutcome::default())
    }
}

#[async_trait]
impl AtomicOperationHandler for CountingHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        _operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AtomicOperationOutcome::default())
    }
}

#[async_trait]
impl AtomicOperationHandler for LocalIdHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match operation {
            PlannedOperation::AddResource { data, .. } => Ok(AtomicOperationOutcome {
                result: AtomicResult {
                    data: Some(json!({"type": data.type_name, "id": "created"})),
                    meta: None,
                },
                created_resource: Some(ResourceIdentifier {
                    type_name: data.type_name.clone(),
                    id: Some("created".to_owned()),
                    ..ResourceIdentifier::default()
                }),
            }),
            PlannedOperation::RemoveResource {
                target: seamark::atomic::AtomicTarget::Reference(reference),
            } => {
                let identity = local_ids.resolve(&ResourceIdentifier {
                    type_name: reference.type_name.clone(),
                    id: reference.id.clone(),
                    lid: reference.lid.clone(),
                    ..ResourceIdentifier::default()
                })?;
                if identity.type_name != "authors"
                    || identity.id.as_deref() != Some("created")
                    || identity.lid.is_some()
                {
                    return Err("unexpected operation or unresolved local ID".to_owned());
                }
                Ok(AtomicOperationOutcome::default())
            }
            _ => Err("unexpected operation or unresolved local ID".to_owned()),
        }
    }
}

struct TestHrefResolver {
    base_url: &'static str,
}

impl TestHrefResolver {
    fn matches_route(&self, href: &str, path: &str) -> bool {
        href == path || href == format!("{}{path}", self.base_url)
    }
}

impl AtomicHrefResolver for TestHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if self.matches_route(href, "/articles/1/relationships/author") {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some("1".to_owned()),
                lid: None,
                relationship: Some("author".to_owned()),
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_resource(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if self.matches_route(href, "/articles/1") {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some("1".to_owned()),
                lid: None,
                relationship: None,
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_collection(&self, href: &str) -> Result<Option<String>, String> {
        Ok(self
            .matches_route(href, "/articles")
            .then(|| "articles".to_owned()))
    }
}

#[async_trait]
impl AtomicOperationHandler for TestHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if self.fail {
            return Err("injected failure".to_owned());
        }
        if self.require_resolved_targets {
            match operation {
                PlannedOperation::AddResource { href: None, .. }
                | PlannedOperation::UpdateResource {
                    target: seamark::atomic::AtomicTarget::Reference(_),
                    ..
                }
                | PlannedOperation::RemoveResource {
                    target: seamark::atomic::AtomicTarget::Reference(_),
                }
                | PlannedOperation::UpdateRelationship {
                    reference:
                        AtomicResourceReference {
                            relationship: Some(_),
                            ..
                        },
                    ..
                } => {}
                _ => return Err("href target was not resolved before execution".to_owned()),
            }
        }
        let result = match operation {
            PlannedOperation::AddResource { data, .. } => AtomicResult {
                data: Some(json!({"type": data.type_name, "id": "created"})),
                meta: None,
            },
            _ => AtomicResult::default(),
        };
        Ok(AtomicOperationOutcome {
            result,
            created_resource: None,
        })
    }
}

fn registry() -> Arc<ResourceRegistry> {
    Arc::new(
        ResourceRegistry::new([
            ResourceDefinition::new("authors", "id").attribute("name", "name", false, false),
            ResourceDefinition::new("articles", "id")
                .attribute("title", "title", false, false)
                .relationship("author", "author_id", "authors"),
        ])
        .unwrap(),
    )
}

fn relationship_add_registry() -> Arc<ResourceRegistry> {
    Arc::new(
        ResourceRegistry::new([
            ResourceDefinition::new("articles", "id").relationship("tags", "tag_ids", "tags"),
            ResourceDefinition::new("tags", "id"),
        ])
        .unwrap(),
    )
}

async fn database() -> DatabaseConnection {
    let url = std::env::var("SEAMARK_TEST_DATABASE_URL")
        .expect("set SEAMARK_TEST_DATABASE_URL to a dedicated PostgreSQL test database");
    Database::connect(url).await.unwrap()
}

fn request(uri: &str, content_type: &str, accept: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT, accept)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn document(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn error_document(response: Response<Body>, request_body: &str) -> Value {
    let expected_status = response.status().as_u16().to_string();
    let document = document(response).await;
    let errors = document["errors"]
        .as_array()
        .expect("error response document");
    assert_eq!(errors.len(), 1);
    let request_document: Option<Value> = serde_json::from_str(request_body).ok();
    for error in errors {
        assert_eq!(error["status"], expected_status);
        if let Some(pointer) = error["source"]["pointer"].as_str() {
            assert!(
                request_document
                    .as_ref()
                    .and_then(|document| document.pointer(pointer))
                    .is_some(),
                "error pointer {pointer} must resolve in the request document"
            );
        }
    }
    document
}

#[tokio::test]
async fn negotiates_and_executes_atomic_http_requests() {
    let database = database().await;
    let app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: false,
        }),
    );
    let valid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"Ada"}}}]}"#;

    let response = app
        .clone()
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    assert_eq!(
        document(response).await,
        json!({"atomic:results": [{"data": {"type": "authors", "id": "created"}}]})
    );

    for (uri, content_type, accept, body, status) in [
        (
            "/operations",
            "application/vnd.api+json",
            ATOMIC_MEDIA_TYPE,
            valid_body,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "/operations",
            "application/vnd.api+json;ext=https://jsonapi.org/ext/atomic",
            ATOMIC_MEDIA_TYPE,
            valid_body,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            "application/vnd.api+json",
            valid_body,
            StatusCode::NOT_ACCEPTABLE,
        ),
        (
            "/operations?unsupported=1",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            "{}",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"[ [{"op":"add","data":{"type":"authors","attributes":{"name":"Ada"}}}] ]"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[["add"]]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"remove","ref":["authors","1"]}]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"add","data":["authors"]}]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            "{",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"Ada","name":"Grace"}}}]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"unknown"},{"op":"also-unknown"}]}"#,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(uri, content_type, accept, body))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            status,
            "{uri} with {content_type} / {accept}"
        );
        let content_type = response.headers()[CONTENT_TYPE].to_str().unwrap();
        assert!(content_type == "application/vnd.api+json" || content_type == ATOMIC_MEDIA_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        let error = error_document(response, body).await;
        assert!(error.get("errors").is_some());
    }

    let malformed_operation = r#"{"atomic:operations":[{"op":"unknown"},{"op":"also-unknown"}]}"#;
    let response = app
        .clone()
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            malformed_operation,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = error_document(response, malformed_operation).await;
    assert_eq!(error["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );

    let denied = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: false }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: false,
        }),
    );
    let response = denied
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    error_document(response, valid_body).await;

    let limited = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(LimitedGuard),
        Arc::new(TestHandler {
            fail: true,
            require_resolved_targets: false,
        }),
    );
    let response = limited
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, valid_body).await;
    assert_eq!(error["errors"][0]["status"], "413");

    let failing = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: true,
            require_resolved_targets: false,
        }),
    );
    let response = failing
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, valid_body).await;
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );

    let href_router = atomic_http::router_with_href_resolver(
        registry(),
        database,
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: true,
        }),
        Arc::new(TestHrefResolver {
            base_url: "https://api.example.test",
        }),
    );
    let response = href_router
        .clone()
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"add","href":"https://api.example.test/articles","data":{"type":"articles","attributes":{"title":"Created"}}},{"op":"update","href":"https://api.example.test/articles/1","data":{"type":"articles","attributes":{"title":"Updated"}}},{"op":"remove","href":"https://api.example.test/articles/1"},{"op":"update","href":"https://api.example.test/articles/1/relationships/author","data":null}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        document(response).await,
        json!({"atomic:results": [
            {"data": {"type": "articles", "id": "created"}},
            {},
            {},
            {}
        ]})
    );

    let mismatched_body = r#"{"atomic:operations":[{"op":"update","href":"https://api.example.test/articles/1","data":{"type":"articles","id":"2","attributes":{"title":"Mismatch"}}}]}"#;
    let response = href_router
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            mismatched_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, mismatched_body).await;
    assert_eq!(error["errors"][0]["code"], "invalid_atomic_operation");
    assert_eq!(error["errors"][0]["status"], "400");
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );
    assert_eq!(
        error["errors"][0]["detail"],
        "invalid operation 0 at `/atomic:operations/0`: the operation target identity must match the resource data"
    );
}

#[tokio::test]
async fn atomic_http_accepts_empty_operations_as_a_successful_no_op() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let body = r#"{"atomic:operations":[]}"#;

    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    assert_eq!(document(response).await, json!({"atomic:results": []}));
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_invalid_reference_identity_combinations() {
    let database = database().await;
    let app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: false,
        }),
    );

    for body in [
        r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors"}}]}"#,
        r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1","lid":"local"}}]}"#,
    ] {
        let response = app
            .clone()
            .oneshot(request(
                "/operations",
                ATOMIC_MEDIA_TYPE,
                ATOMIC_MEDIA_TYPE,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = error_document(response, body).await;
        assert_eq!(error["errors"].as_array().unwrap().len(), 1);
        assert_eq!(
            error["errors"][0]["source"]["pointer"], "/atomic:operations/0",
            "error for malformed reference `{body}`: {error}"
        );
    }

    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_requires_a_relationship_ref_for_relationship_adds() {
    let database = database().await;
    let valid_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let valid_guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let valid_app = atomic_http::router(
        relationship_add_registry(),
        database.clone(),
        valid_guard.clone(),
        valid_handler.clone(),
    );
    let valid_body = r#"{"atomic:operations":[{"op":"add","ref":{"type":"articles","id":"1","relationship":"tags"},"data":[{"type":"tags","id":"tag-1"}]}]}"#;
    let response = valid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    let status = response.status();
    let response_body = document(response).await;
    assert_eq!(status, StatusCode::OK, "{response_body}");
    assert_eq!(valid_guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(valid_handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(response_body, json!({"atomic:results": [{}]}));

    let invalid_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let invalid_guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let invalid_app = atomic_http::router(
        relationship_add_registry(),
        database.clone(),
        invalid_guard.clone(),
        invalid_handler.clone(),
    );
    let invalid_body = r#"{"atomic:operations":[{"op":"add","ref":{"type":"articles","id":"1"},"data":[{"type":"tags","id":"tag-1"}]}]}"#;
    let response = invalid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            invalid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    let error = error_document(response, invalid_body).await;
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0/ref"
    );
    assert_eq!(invalid_guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(invalid_handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_points_unknown_resource_attribute_to_nested_data_member() {
    let database = database().await;
    let valid_handler = Arc::new(AtMemberHandler {
        calls: AtomicUsize::new(0),
    });
    let valid_app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        valid_handler.clone(),
    );
    let valid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"Ada"}}}]}"#;
    let response = valid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(valid_handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        document(response).await,
        json!({"atomic:results": [{"data": {"type": "authors", "id": "created"}}]})
    );

    let invalid_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let invalid_app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        invalid_handler.clone(),
    );
    let invalid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"secret":"not registered"}}}]}"#;
    let response = invalid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            invalid_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, invalid_body).await;
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0/data/attributes/secret"
    );
    assert_eq!(invalid_handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_points_unknown_relationship_to_escaped_nested_member() {
    let database = database().await;
    let valid_app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: false,
        }),
    );
    let valid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"articles","relationships":{"author":{"data":{"type":"authors","id":"author-1"}}}}}]}"#;
    let response = valid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        document(response).await,
        json!({"atomic:results": [{"data": {"type": "articles", "id": "created"}}]})
    );

    let invalid_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let invalid_app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        invalid_handler.clone(),
    );
    let invalid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"articles","relationships":{"secret/owner~":{"data":{"type":"authors","id":"author-1"}}}}}]}"#;
    let response = invalid_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            invalid_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, invalid_body).await;
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0/data/relationships/secret~1owner~0"
    );
    assert_eq!(invalid_handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_unresolved_local_ids_before_execution() {
    let database = database().await;
    let local_id_handler = Arc::new(LocalIdHandler {
        calls: AtomicUsize::new(0),
    });
    let local_id_app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        local_id_handler.clone(),
    );
    let valid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","lid":"author-local","attributes":{"name":"Ada"}}},{"op":"remove","ref":{"type":"authors","lid":"author-local"}}]}"#;
    let valid_response = local_id_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();
    assert_eq!(valid_response.status(), StatusCode::OK);
    assert_eq!(
        document(valid_response).await,
        json!({"atomic:results": [
            {"data": {"type": "authors", "id": "created"}},
            {}
        ]})
    );
    assert_eq!(local_id_handler.calls.load(Ordering::SeqCst), 2);

    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );

    for body in [
        r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","lid":"future"}},{"op":"add","data":{"type":"authors","lid":"future"}}]}"#,
        r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","lid":"unknown"}}]}"#,
        r#"{"atomic:operations":[{"op":"update","ref":{"type":"authors","id":"1"},"data":{"type":"authors","lid":"from-update","attributes":{"name":"Updated"}}},{"op":"remove","ref":{"type":"authors","lid":"from-update"}}]}"#,
        r#"{"atomic:operations":[{"op":"update","ref":{"type":"articles","id":"1","relationship":"author"},"data":{"type":"authors","lid":"future"}},{"op":"add","data":{"type":"authors","lid":"future"}}]}"#,
    ] {
        let response = app
            .clone()
            .oneshot(request(
                "/operations",
                ATOMIC_MEDIA_TYPE,
                ATOMIC_MEDIA_TYPE,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = error_document(response, body).await;
        assert_eq!(
            error["errors"][0]["source"]["pointer"],
            "/atomic:operations/0"
        );
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    }

    database.close().await.unwrap();
}

#[tokio::test]
async fn database_failures_return_a_server_error_document() {
    let database = database().await;
    database.clone().close().await.unwrap();
    let app = atomic_http::router(
        registry(),
        database,
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: false,
        }),
    );
    let body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"Ada"}}}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    error_document(response, body).await;
}

#[tokio::test]
async fn execution_failure_pointer_identifies_later_failed_operation() {
    let database = database().await;
    let handler = Arc::new(FailSecondHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        database,
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[{"op":"update","ref":{"type":"authors","id":"1"},"data":{"type":"authors","attributes":{"name":"First"}}},{"op":"update","ref":{"type":"authors","id":"2"},"data":{"type":"authors","attributes":{"name":"Second"}}}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, body).await;
    assert_eq!(error["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/1"
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn atomic_http_ignores_at_members_and_rejects_unknown_attributes() {
    let database = database().await;
    let handler = Arc::new(AtMemberHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        database,
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );
    let valid_body = r#"{"@documentAnnotation":false,"atomic:operations":[{"@operationAnnotation":false,"op":"add","data":{"@resourceAnnotation":false,"type":"authors","attributes":{"@attributeAnnotation":false,"name":"Ada"},"relationships":{"@relationshipAnnotation":false},"links":{"@resourceLink":false,"self":"/authors/created"},"meta":{"@resourceMeta":false}}}]}"#;
    let response = app
        .clone()
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            valid_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    assert_eq!(
        document(response).await,
        json!({"atomic:results": [{"data": {"type": "authors", "id": "created"}}]})
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let invalid_body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","attributes":{"name":"Grace","unknown":"not ignored"}}}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            invalid_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, invalid_body).await;
    assert_eq!(error["errors"].as_array().unwrap().len(), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn atomic_http_enforces_content_type_parameter_rules() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database, guard.clone(), handler.clone());
    let body = r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1"}}]}"#;
    let content_type = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/unknown\"";
    let response = app
        .clone()
        .oneshot(request(
            "/operations",
            content_type,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    document(response).await;
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    for content_type in [
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/unknown\";charset=utf-8",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";version=1",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic https://example.test/unsupported\"",
    ] {
        let response = app
            .clone()
            .oneshot(request(
                "/operations",
                content_type,
                ATOMIC_MEDIA_TYPE,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/vnd.api+json");
        assert_eq!(response.headers()[VARY], "Accept");
        assert_eq!(
            error_document(response, body).await["errors"][0]["code"],
            "unsupported_media_type"
        );
        assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }
}
