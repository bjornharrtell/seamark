#![allow(missing_docs)]

use std::sync::Arc;

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
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};
use tower::ServiceExt;

const ATOMIC_MEDIA_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

struct TestGuard {
    allowed: bool,
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

struct TestHandler {
    fail: bool,
    require_resolved_targets: bool,
}

struct TestHrefResolver;

impl AtomicHrefResolver for TestHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if href == "/articles/1/relationships/author" {
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
        if href == "/articles/1" {
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
        Ok((href == "/articles").then(|| "articles".to_owned()))
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
            "{",
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
    error_document(response, valid_body).await;

    let href_router = atomic_http::router_with_href_resolver(
        registry(),
        database,
        Arc::new(TestGuard { allowed: true }),
        Arc::new(TestHandler {
            fail: false,
            require_resolved_targets: true,
        }),
        Arc::new(TestHrefResolver),
    );
    let response = href_router
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            r#"{"atomic:operations":[{"op":"add","href":"/articles","data":{"type":"articles","attributes":{"title":"Created"}}},{"op":"update","href":"/articles/1","data":{"type":"articles","attributes":{"title":"Updated"}}},{"op":"remove","href":"/articles/1"},{"op":"update","href":"/articles/1/relationships/author","data":null}]}"#,
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
}
