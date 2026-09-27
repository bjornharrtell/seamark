#![allow(missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderValue, Method, Request, Response, StatusCode};
use sea_orm::{
    ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, DatabaseTransaction, Statement,
};
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

struct AuthorizationOrderGuard {
    allowed: bool,
    authorize_calls: Arc<AtomicUsize>,
    limit_calls: Arc<AtomicUsize>,
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

#[async_trait]
impl AtomicOperationsGuard for AuthorizationOrderGuard {
    async fn authorize(
        &self,
        _headers: &axum::http::HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        self.allowed
    }

    fn validate_limits(&self, _operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        self.limit_calls.fetch_add(1, Ordering::SeqCst);
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

struct RelativeHrefResolver {
    calls: Arc<Mutex<Vec<String>>>,
}

impl AtomicHrefResolver for RelativeHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        self.calls.lock().unwrap().push(href.to_owned());
        Ok(None)
    }

    fn resolve_resource(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        self.calls.lock().unwrap().push(href.to_owned());
        Ok((href == "articles/1").then(|| AtomicResourceReference {
            type_name: "articles".to_owned(),
            id: Some("1".to_owned()),
            lid: None,
            relationship: None,
        }))
    }
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

struct AdditionalUpdateResultHandler {
    calls: AtomicUsize,
}

struct RelationshipResultHandler {
    calls: AtomicUsize,
}

struct LocalIdHandler {
    calls: AtomicUsize,
}

struct MissingCreatedIdentityHandler {
    calls: AtomicUsize,
}

struct DeferredCommitFailureHandler {
    parent_table: String,
    child_table: String,
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
impl AtomicOperationHandler for AdditionalUpdateResultHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if !matches!(operation, PlannedOperation::UpdateResource { .. }) {
            return Err("expected a resource update".to_owned());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({
                    "type": "authors",
                    "id": "1",
                    "attributes": {
                        "name": "Grace",
                        "revision": 2
                    }
                })),
                meta: None,
            },
            created_resource: None,
        })
    }
}

#[async_trait]
impl AtomicOperationHandler for DeferredCommitFailureHandler {
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        _operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        transaction
            .execute_unprepared(&format!(
                "INSERT INTO {} (id) VALUES (1)",
                self.parent_table
            ))
            .await
            .map_err(|error| error.to_string())?;
        transaction
            .execute_unprepared(&format!(
                "INSERT INTO {} (parent_id) VALUES (999)",
                self.child_table
            ))
            .await
            .map_err(|error| error.to_string())?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AtomicOperationOutcome::default())
    }
}

#[async_trait]
impl AtomicOperationHandler for RelationshipResultHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        if !matches!(operation, PlannedOperation::UpdateRelationship { .. }) {
            return Err("expected a relationship update".to_owned());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": "authors", "id": "1"})),
                meta: None,
            },
            created_resource: None,
        })
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

#[async_trait]
impl AtomicOperationHandler for MissingCreatedIdentityHandler {
    async fn execute_operation(
        &self,
        _transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let PlannedOperation::AddResource { data, .. } = operation else {
            return Err("expected a resource add".to_owned());
        };
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": data.type_name, "id": "created"})),
                meta: None,
            },
            created_resource: None,
        })
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

fn update_result_registry() -> Arc<ResourceRegistry> {
    Arc::new(
        ResourceRegistry::new([ResourceDefinition::new("authors", "id")
            .attribute("name", "name", false, false)
            .attribute("revision", "revision", false, false)])
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
async fn atomic_http_denial_precedes_transaction_and_operation_handler() {
    let body = r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1"}}]}"#;
    let denied_database = DatabaseConnection::default();
    let denied_authorize_calls = Arc::new(AtomicUsize::new(0));
    let denied_limit_calls = Arc::new(AtomicUsize::new(0));
    let denied_guard = Arc::new(AuthorizationOrderGuard {
        allowed: false,
        authorize_calls: denied_authorize_calls.clone(),
        limit_calls: denied_limit_calls.clone(),
    });
    let denied_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let denied_app = atomic_http::router(
        registry(),
        denied_database,
        denied_guard,
        denied_handler.clone(),
    );
    let denied_response = denied_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(denied_response.status(), StatusCode::FORBIDDEN);
    assert_eq!(denied_response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(denied_response.headers()[VARY], "Accept");
    let denied_document = error_document(denied_response, body).await;
    assert_eq!(
        denied_document["errors"],
        json!([{
            "status": "403",
            "code": "forbidden",
            "title": "Access denied",
            "detail": "The Atomic Operations request is not authorized."
        }])
    );
    assert_eq!(denied_authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(denied_limit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(denied_handler.calls.load(Ordering::SeqCst), 0);

    let authorized_database = database().await;
    let authorized_authorize_calls = Arc::new(AtomicUsize::new(0));
    let authorized_limit_calls = Arc::new(AtomicUsize::new(0));
    let authorized_guard = Arc::new(AuthorizationOrderGuard {
        allowed: true,
        authorize_calls: authorized_authorize_calls.clone(),
        limit_calls: authorized_limit_calls.clone(),
    });
    let authorized_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let authorized_app = atomic_http::router(
        registry(),
        authorized_database.clone(),
        authorized_guard,
        authorized_handler.clone(),
    );
    let authorized_response = authorized_app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(authorized_response.status(), StatusCode::OK);
    assert_eq!(
        authorized_response.headers()[CONTENT_TYPE],
        ATOMIC_MEDIA_TYPE
    );
    assert_eq!(authorized_response.headers()[VARY], "Accept");
    assert_eq!(
        document(authorized_response).await,
        json!({"atomic:results": [{}]})
    );
    assert_eq!(authorized_authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(authorized_limit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(authorized_handler.calls.load(Ordering::SeqCst), 1);
    authorized_database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_returns_jsonapi_error_for_unsupported_method() {
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        DatabaseConnection::default(),
        guard.clone(),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[]}"#;
    let mut request = request("/operations", ATOMIC_MEDIA_TYPE, ATOMIC_MEDIA_TYPE, body);
    *request.method_mut() = Method::GET;
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    assert!(response.headers().contains_key("allow"));
    let error = error_document(response, body).await;
    assert_eq!(error["errors"][0]["status"], "405");
    assert_eq!(error["errors"][0]["code"], "method_not_allowed");
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
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

    let limited_handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let limited = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(LimitedGuard),
        limited_handler.clone(),
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
    assert_eq!(
        error["errors"][0],
        json!({
            "status": "413",
            "code": "resource_limit",
            "title": "Atomic Operations request exceeds limits",
            "detail": "operation limit exceeded"
        })
    );
    assert!(error.get("atomic:results").is_none());
    assert_eq!(limited_handler.calls.load(Ordering::SeqCst), 0);

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
    assert_eq!(error["errors"][0]["status"], "422");
    assert_eq!(error["errors"][0]["code"], "operation_failed");
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
async fn atomic_http_rejects_non_request_members_in_operations_request() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    for body in [
        r#"{"atomic:operations":[],"errors":[{"title":"bad"}]}"#,
        r#"{"atomic:operations":[],"atomic:results":[]}"#,
        r#"{"atomic:operations":[],"data":null}"#,
        r#"{"atomic:operations":[],"included":[]}"#,
        r#"{"atomic:operations":[],"jsonapi":{"profile":["relative/profile"]}}"#,
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
        assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        let error = error_document(response, body).await;
        assert_eq!(error["errors"].as_array().unwrap().len(), 1);
        assert_eq!(error["errors"][0]["code"], "invalid_atomic_operation");
    }
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_missing_operation_data_before_authorization_or_handler() {
    let database = database().await;
    let authorize_calls = Arc::new(AtomicUsize::new(0));
    let limit_calls = Arc::new(AtomicUsize::new(0));
    let guard = Arc::new(AuthorizationOrderGuard {
        allowed: true,
        authorize_calls: authorize_calls.clone(),
        limit_calls: limit_calls.clone(),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard, handler.clone());
    for (body, detail) in [
        (
            r#"{"atomic:operations":[{"op":"add"}]}"#,
            "an add operation requires `data`",
        ),
        (
            r#"{"atomic:operations":[{"op":"update","ref":{"type":"authors","id":"1"}}]}"#,
            "an update operation requires `data`",
        ),
        (
            r#"{"atomic:operations":[{"op":"add","ref":{"type":"articles","id":"1","relationship":"author"}}]}"#,
            "an add operation requires `data`",
        ),
        (
            r#"{"atomic:operations":[{"op":"update","ref":{"type":"articles","id":"1","relationship":"author"}}]}"#,
            "an update operation requires `data`",
        ),
        (
            r#"{"atomic:operations":[{"op":"remove","ref":{"type":"articles","id":"1","relationship":"author"}}]}"#,
            "relationship operation requires `data`",
        ),
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
        assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        let error = error_document(response, body).await;
        assert!(error.get("atomic:results").is_none());
        assert_eq!(
            error,
            json!({
                "errors": [{
                    "code": "invalid_atomic_operation",
                    "title": "Invalid Atomic Operations request",
                    "detail": format!("invalid operation 0 at `/atomic:operations/0`: {detail}"),
                    "status": "400",
                    "source": {"pointer": "/atomic:operations/0"}
                }]
            })
        );
    }
    assert_eq!(authorize_calls.load(Ordering::SeqCst), 0);
    assert_eq!(limit_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_resource_remove_with_data_before_authorization_or_handler() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let body =
        r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1"},"data":null}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, body).await;
    assert!(error.get("atomic:results").is_none());
    assert_eq!(
        error["errors"][0]["detail"],
        "invalid operation 0 at `/atomic:operations/0`: removing a resource must not include `data`"
    );
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_passes_relative_href_unchanged_to_application_resolver() {
    let database = database().await;
    let resolver_calls = Arc::new(Mutex::new(Vec::new()));
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router_with_href_resolver(
        registry(),
        database.clone(),
        guard.clone(),
        handler.clone(),
        Arc::new(RelativeHrefResolver {
            calls: resolver_calls.clone(),
        }),
    );
    let body = r#"{"atomic:operations":[{"op":"update","href":"articles/1","data":{"type":"articles","attributes":{"title":"Updated"}}}]}"#;

    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    let status = response.status();
    let content_type = response.headers()[CONTENT_TYPE].clone();
    let vary = response.headers()[VARY].clone();
    let response_document = document(response).await;
    assert_eq!(status, StatusCode::OK, "{response_document}");
    assert_eq!(content_type, ATOMIC_MEDIA_TYPE);
    assert_eq!(vary, "Accept");
    assert_eq!(response_document, json!({"atomic:results": [{}]}));
    assert_eq!(
        *resolver_calls.lock().unwrap(),
        vec!["articles/1", "articles/1"]
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
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
async fn atomic_http_rejects_relationship_cardinality_mismatch_before_authorization() {
    let database = database().await;
    let registry = ResourceRegistry::new([
        ResourceDefinition::new("authors", "author_id"),
        ResourceDefinition::new("articles", "article_id")
            .to_one_relationship("author", "author_id", "authors")
            .to_many_relationship("tags", "tag_ids", "tags"),
        ResourceDefinition::new("tags", "tag_id"),
    ])
    .unwrap();
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        Arc::new(registry),
        database.clone(),
        guard.clone(),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[{"op":"add","ref":{"type":"articles","id":"1","relationship":"author"},"data":[{"type":"authors","id":"2"}]}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(
        error_document(response, body).await["errors"][0]["source"]["pointer"],
        "/atomic:operations/0/data"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_non_array_relationship_add_and_remove_data_before_authorization() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        relationship_add_registry(),
        database.clone(),
        guard.clone(),
        handler.clone(),
    );

    for (body, message) in [
        (
            r#"{"atomic:operations":[{"op":"add","ref":{"type":"articles","id":"1","relationship":"tags"},"data":{"type":"tags","id":"tag-1"}}]}"#,
            "adding relationship members requires an array of identifiers",
        ),
        (
            r#"{"atomic:operations":[{"op":"remove","ref":{"type":"articles","id":"1","relationship":"tags"},"data":null}]}"#,
            "removing relationship members requires an array of identifiers",
        ),
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
        assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        let error = error_document(response, body).await;
        assert!(error.get("atomic:results").is_none());
        assert_eq!(
            error["errors"][0]["detail"],
            format!("invalid operation 0 at `/atomic:operations/0`: {message}")
        );
        assert_eq!(
            error["errors"][0]["source"]["pointer"],
            "/atomic:operations/0"
        );
    }
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_operations_with_both_ref_and_href_before_execution() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let body = r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1"},"href":"/author-resource/1"}]}"#;

    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, body).await;
    assert!(error.get("atomic:results").is_none());
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_resource_remove_without_target_before_authorization_or_handler() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let body = r#"{"atomic:operations":[{"op":"remove"}]}"#;
    let response = app
        .oneshot(request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            ATOMIC_MEDIA_TYPE,
            body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    let error = error_document(response, body).await;
    assert_eq!(
        error["errors"][0]["detail"],
        "invalid operation 0 at `/atomic:operations/0`: remove requires `ref` or `href`"
    );
    assert_eq!(
        error["errors"][0]["source"]["pointer"],
        "/atomic:operations/0"
    );
    assert!(error.get("atomic:results").is_none());
    assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
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
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database, guard.clone(), handler.clone());
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
    let error = error_document(response, body).await;
    assert_eq!(error["errors"][0]["code"], "database_error");
    assert_eq!(error["errors"][0]["status"], "500");
    assert!(error["errors"][0].get("source").is_none());
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn deferred_constraint_commit_failure_returns_server_error_and_rolls_back_writes() {
    let database = database().await;
    let suffix = std::process::id();
    let parent_table = format!("seamark_atomic_commit_parent_{suffix}");
    let child_table = format!("seamark_atomic_commit_child_{suffix}");
    database
        .execute_unprepared(&format!("DROP TABLE IF EXISTS {child_table}"))
        .await
        .unwrap();
    database
        .execute_unprepared(&format!("DROP TABLE IF EXISTS {parent_table}"))
        .await
        .unwrap();
    database
        .execute_unprepared(&format!(
            "CREATE TABLE {parent_table} (id INTEGER PRIMARY KEY)"
        ))
        .await
        .unwrap();
    database
        .execute_unprepared(&format!(
            "CREATE TABLE {child_table} (parent_id INTEGER REFERENCES {parent_table}(id) DEFERRABLE INITIALLY DEFERRED)"
        ))
        .await
        .unwrap();

    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(DeferredCommitFailureHandler {
        parent_table: parent_table.clone(),
        child_table: child_table.clone(),
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let body = r#"{"atomic:operations":[{"op":"remove","ref":{"type":"authors","id":"1"}}]}"#;
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
    let error = error_document(response, body).await;
    assert_eq!(error["errors"][0]["code"], "database_error");
    assert_eq!(error["errors"][0]["status"], "500");
    assert!(error["errors"][0].get("source").is_none());
    assert!(error.get("atomic:results").is_none());
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    for table in [&parent_table, &child_table] {
        let row = database
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
    }

    database
        .execute_unprepared(&format!("DROP TABLE {child_table}"))
        .await
        .unwrap();
    database
        .execute_unprepared(&format!("DROP TABLE {parent_table}"))
        .await
        .unwrap();
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
    assert!(error.get("atomic:results").is_none());
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
    let content_type = r#"APPLICATION/VND.API+JSON ; EXT = "https://jsonapi.org/ext/\atomic" ; PROFILE = "https://example.test/\unknown;version=1 https://example.test/profiles/a,b https://example.test/also-unknown""#;
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
        "application/vnd.api+json",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";unknown",
        "application/vnd.api+json;ext=\"\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic  https://example.test/other\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/one  https://example.test/two\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/unknown\";charset=utf-8",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";version=1",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic https://example.test/unsupported\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";ext=\"https://jsonapi.org/ext/atomic\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/one\";profile=\"https://example.test/two\"",
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

    let mut duplicate_content_type = request("/operations", content_type, ATOMIC_MEDIA_TYPE, body);
    duplicate_content_type
        .headers_mut()
        .append(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let response = app.oneshot(duplicate_content_type).await.unwrap();
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

#[tokio::test]
async fn atomic_http_negotiates_qvalues_wildcards_and_extension_parameters() {
    let database = database().await;
    let body = r#"{"atomic:operations":[]}"#;
    let accepted = [
        ATOMIC_MEDIA_TYPE,
        r#"application/vnd.api+json;ext="https://jsonapi.org/ext/\atomic""#,
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0.500",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1.",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1.000;foo=bar",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1;foo",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1;foo=\"\"",
        r#"application/vnd.api+json;ext="https://jsonapi.org/ext/atomic";q=1;foo="x\"y""#,
        "application/*;ext=\"https://jsonapi.org/ext/atomic\";q=0.7",
        "*/*;ext=\"https://jsonapi.org/ext/atomic\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/one https://example.test/two\";q=1",
        "application/vnd.api+json ; ext = \"https://jsonapi.org/ext/atomic\" ; profile = \"https://example.test/profiles/a,b\" ; q = 1",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0,application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0.7",
    ];
    let rejected = [
        "application/vnd.api+json",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1.001",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0.1234",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=1e0",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=+1",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=.5",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=\"0.5\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic https://example.test/other\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";ext=\"https://jsonapi.org/ext/atomic\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/one\";profile=\"https://example.test/two\"",
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";foo;q=1",
        r#"application/vnd.api+json;ext="https://jsonapi.org/ext/atomic";q=1;foo="x\""#,
        r#"application/vnd.api+json;ext="https://jsonapi.org/ext/atomic";q=1;foo="x"y"z""#,
        "application/*",
        "*/*;q=1",
        "*/*;ext=\"https://jsonapi.org/ext/atomic\";q=1,application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0",
    ];

    for accept in accepted {
        let guard = Arc::new(CountingGuard {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler {
            calls: AtomicUsize::new(0),
        });
        let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
        let response = app
            .oneshot(request("/operations", ATOMIC_MEDIA_TYPE, accept, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        assert_eq!(document(response).await, json!({"atomic:results": []}));
        assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    }

    for accept in rejected {
        let guard = Arc::new(CountingGuard {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler {
            calls: AtomicUsize::new(0),
        });
        let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
        let response = app
            .oneshot(request("/operations", ATOMIC_MEDIA_TYPE, accept, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE, "{accept}");
        assert_eq!(response.headers()[CONTENT_TYPE], "application/vnd.api+json");
        assert_eq!(response.headers()[VARY], "Accept");
        let error = error_document(response, body).await;
        assert_eq!(error["errors"][0]["status"], "406");
        assert_eq!(error["errors"][0]["code"], "not_acceptable");
        assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    }

    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_combines_repeated_accept_header_fields() {
    let database = database().await;
    let guard = Arc::new(CountingGuard {
        calls: AtomicUsize::new(0),
    });
    let handler = Arc::new(CountingHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
    let mut repeated_exact_request = request(
        "/operations",
        ATOMIC_MEDIA_TYPE,
        "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0",
        r#"{"atomic:operations":[]}"#,
    );
    repeated_exact_request.headers_mut().append(
        ACCEPT,
        HeaderValue::from_static(
            "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0.7",
        ),
    );

    let response = app.oneshot(repeated_exact_request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
    assert_eq!(response.headers()[VARY], "Accept");
    assert_eq!(document(response).await, json!({"atomic:results": []}));
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);

    for (wildcard_quality, exact_quality, expected_status) in [
        ("1", "0", StatusCode::NOT_ACCEPTABLE),
        ("1", "0.4", StatusCode::OK),
    ] {
        let guard = Arc::new(CountingGuard {
            calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler {
            calls: AtomicUsize::new(0),
        });
        let app = atomic_http::router(registry(), database.clone(), guard.clone(), handler.clone());
        let mut request = request(
            "/operations",
            ATOMIC_MEDIA_TYPE,
            &format!("application/*;ext=\"https://jsonapi.org/ext/atomic\";q={wildcard_quality}"),
            r#"{"atomic:operations":[]}"#,
        );
        request.headers_mut().append(
            ACCEPT,
            HeaderValue::from_str(&format!(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q={exact_quality}"
            ))
            .unwrap(),
        );

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), expected_status);
        assert_eq!(response.headers()[VARY], "Accept");
        if expected_status == StatusCode::OK {
            assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_MEDIA_TYPE);
            assert_eq!(document(response).await, json!({"atomic:results": []}));
            assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
        } else {
            let error = error_document(response, r#"{"atomic:operations":[]}"#).await;
            assert_eq!(error["errors"][0]["code"], "not_acceptable");
            assert_eq!(guard.calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    }
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_rejects_relationship_result_data_with_operation_pointer() {
    let database = database().await;
    let handler = Arc::new(RelationshipResultHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[{"op":"update","ref":{"type":"articles","id":"1","relationship":"author"},"data":null}]}"#;
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
    let error = error_document(response, body).await;
    assert_eq!(
        error["errors"][0],
        json!({
            "status": "500",
            "code": "invalid_atomic_response",
            "title": "Atomic Operations response failed validation",
            "detail": "this operation result must not contain `data`",
            "source": {"pointer": "/atomic:operations/0"}
        })
    );
    assert!(error.get("atomic:results").is_none());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_returns_updated_resource_for_additional_server_fields() {
    let database = database().await;
    let handler = Arc::new(AdditionalUpdateResultHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        update_result_registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[{"op":"update","ref":{"type":"authors","id":"1"},"data":{"type":"authors","attributes":{"name":"Grace"}}}]}"#;
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
    assert_eq!(
        document(response).await,
        json!({
            "atomic:results": [{
                "data": {
                    "type": "authors",
                    "id": "1",
                    "attributes": {
                        "name": "Grace",
                        "revision": 2
                    }
                }
            }]
        })
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    database.close().await.unwrap();
}

#[tokio::test]
async fn atomic_http_maps_missing_created_local_id_identity_to_server_error() {
    let database = database().await;
    let handler = Arc::new(MissingCreatedIdentityHandler {
        calls: AtomicUsize::new(0),
    });
    let app = atomic_http::router(
        registry(),
        database.clone(),
        Arc::new(TestGuard { allowed: true }),
        handler.clone(),
    );
    let body = r#"{"atomic:operations":[{"op":"add","data":{"type":"authors","lid":"author-one","attributes":{"name":"Ada"}}}]}"#;
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
    let error = error_document(response, body).await;
    assert_eq!(
        error["errors"][0],
        json!({
            "status": "500",
            "code": "atomic_local_id_mapping_failed",
            "title": "Atomic local ID mapping failed",
            "detail": "add operation did not return an identity for lid `author-one`",
            "source": {"pointer": "/atomic:operations/0"}
        })
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    database.close().await.unwrap();
}
