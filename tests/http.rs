#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use seamark::document::{JsonApiDocument, Relationship, RelationshipData, ResourceIdentifier};
use seamark::http::{self, AdapterError, AdapterResource, RequestAuthorizer, ResourceAdapter};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};
use tower::ServiceExt;

const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";

#[derive(Default)]
struct TestAdapter {
    collection_result: Mutex<Vec<AdapterResource>>,
    resource_result: Mutex<Option<AdapterResource>>,
    fail_collection: AtomicBool,
    fail_resource: AtomicBool,
    identifier_fields: Mutex<Vec<String>>,
    collection_calls: AtomicUsize,
    resource_calls: AtomicUsize,
}

#[async_trait]
impl ResourceAdapter for TestAdapter {
    async fn collection(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<Vec<AdapterResource>, AdapterError> {
        self.collection_calls.fetch_add(1, Ordering::SeqCst);
        self.identifier_fields
            .lock()
            .unwrap()
            .push(resource.identifier_field().to_owned());
        if self.fail_collection.load(Ordering::SeqCst) {
            return Err(AdapterError);
        }
        Ok(self.collection_result.lock().unwrap().clone())
    }

    async fn resource(
        &self,
        resource: &ResourceDefinition,
        _id: &str,
    ) -> Result<Option<AdapterResource>, AdapterError> {
        self.resource_calls.fetch_add(1, Ordering::SeqCst);
        self.identifier_fields
            .lock()
            .unwrap()
            .push(resource.identifier_field().to_owned());
        if self.fail_resource.load(Ordering::SeqCst) {
            return Err(AdapterError);
        }
        Ok(self.resource_result.lock().unwrap().clone())
    }
}

struct TestAuthorizer {
    allowed: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl RequestAuthorizer for TestAuthorizer {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &axum::http::HeaderMap,
    ) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.allowed
    }
}

fn port_record() -> AdapterResource {
    AdapterResource {
        id: "1".to_owned(),
        attributes: BTreeMap::from([
            ("title".to_owned(), Value::Null),
            ("secret".to_owned(), json!("must not leak")),
        ]),
        relationships: BTreeMap::from([
            (
                "owner".to_owned(),
                Relationship {
                    data: Some(RelationshipData::One(ResourceIdentifier {
                        type_name: "people".to_owned(),
                        id: Some("3".to_owned()),
                        ..ResourceIdentifier::default()
                    })),
                    ..Relationship::default()
                },
            ),
            (
                "hidden_owner".to_owned(),
                Relationship {
                    data: Some(RelationshipData::One(ResourceIdentifier {
                        type_name: "people".to_owned(),
                        id: Some("4".to_owned()),
                        ..ResourceIdentifier::default()
                    })),
                    ..Relationship::default()
                },
            ),
        ]),
    }
}

fn test_app(adapter: Arc<TestAdapter>, allowed: bool) -> (Router, Arc<TestAuthorizer>) {
    let ports = ResourceDefinition::new("ports", "port_key")
        .attribute("name", "title", true, true)
        .relationship("owner", "owner", "people");
    let people =
        ResourceDefinition::new("people", "id").attribute("name", "full_name", false, false);
    let registry = Arc::new(ResourceRegistry::new([ports, people]).unwrap());
    let authorizer = Arc::new(TestAuthorizer {
        allowed,
        calls: AtomicUsize::new(0),
    });
    (
        http::router(registry, adapter, authorizer.clone()),
        authorizer,
    )
}

fn request(uri: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(accept) = accept {
        builder = builder.header(ACCEPT, accept);
    }
    builder.body(Body::empty()).unwrap()
}

fn request_with_accepts(uri: &str, accepts: &[&str]) -> Request<Body> {
    let mut request = request(uri, None);
    for accept in accepts {
        request
            .headers_mut()
            .append(ACCEPT, HeaderValue::from_str(accept).unwrap());
    }
    request
}

async fn document(response: Response<Body>) -> JsonApiDocument {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn assert_jsonapi_headers(response: &Response<Body>) {
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        JSONAPI_MEDIA_TYPE
    );
    assert_eq!(response.headers().get(VARY).unwrap(), "Accept");
}

#[tokio::test]
async fn collection_projects_only_declared_fields_and_preserves_nulls() {
    let adapter = Arc::new(TestAdapter::default());
    *adapter.collection_result.lock().unwrap() = vec![port_record()];
    let (app, authorizer) = test_app(adapter.clone(), true);

    let response = app.oneshot(request("/ports", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    let document = document(response).await;

    assert_eq!(
        serde_json::to_value(document).unwrap(),
        json!({
            "data": [{
                "type": "ports",
                "id": "1",
                "attributes": {"name": null},
                "relationships": {
                    "owner": {"data": {"type": "people", "id": "3"}}
                }
            }]
        })
    );
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        adapter.identifier_fields.lock().unwrap().as_slice(),
        ["port_key"]
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn single_resource_route_returns_one_resource() {
    let adapter = Arc::new(TestAdapter::default());
    *adapter.resource_result.lock().unwrap() = Some(port_record());
    let (app, _) = test_app(adapter.clone(), true);

    let response = app
        .oneshot(request("/ports/1", Some(JSONAPI_MEDIA_TYPE)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    let document = document(response).await;

    assert_eq!(
        serde_json::to_value(document).unwrap()["data"],
        json!({
            "type": "ports",
            "id": "1",
            "attributes": {"name": null},
            "relationships": {
                "owner": {"data": {"type": "people", "id": "3"}}
            }
        })
    );
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        adapter.identifier_fields.lock().unwrap().as_slice(),
        ["port_key"]
    );
}

#[tokio::test]
async fn empty_collection_is_an_empty_data_array() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter, true);

    let response = app.oneshot(request("/ports", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    assert_eq!(
        serde_json::to_value(document(response).await).unwrap(),
        json!({"data": []})
    );
}

#[tokio::test]
async fn missing_single_resource_returns_a_structured_not_found_error() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter.clone(), true);

    let response = app.oneshot(request("/ports/missing", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "resource_not_found");
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_resource_type_returns_a_structured_error_without_adapter_calls() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter.clone(), true);

    let response = app.oneshot(request("/ships", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "unknown_resource_type");
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn authorization_denial_precedes_adapter_calls_for_both_routes() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        let (app, authorizer) = test_app(adapter.clone(), false);

        let response = app.oneshot(request(path, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_jsonapi_headers(&response);
        let body = serde_json::to_value(document(response).await).unwrap();
        assert_eq!(body["errors"][0]["code"], "forbidden");
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn media_negotiation_is_applied_to_both_routes() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter.clone(), true);

        let response = app
            .oneshot(request(path, Some("text/plain")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
        assert_jsonapi_headers(&response);
        let body = serde_json::to_value(document(response).await).unwrap();
        assert_eq!(body["errors"][0]["code"], "not_acceptable");
        assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn supports_jsonapi_and_wildcard_accept_ranges_on_both_routes() {
    for (path, accept) in [
        ("/ports", JSONAPI_MEDIA_TYPE),
        ("/ports/1", "application/*"),
        ("/ports", "*/*"),
    ] {
        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter, true);

        let response = app.oneshot(request(path, Some(accept))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_jsonapi_headers(&response);
    }
}

#[tokio::test]
async fn ignores_profile_parameters_and_rejects_unsupported_extensions() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter, true);
        let response = app
            .oneshot(request(
                path,
                Some("application/vnd.api+json;profile=\"https://example.test/unknown\""),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_jsonapi_headers(&response);
    }

    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter, true);
    let response = app
        .oneshot(request(
            "/ports",
            Some("application/vnd.api+json;ext=\"https://example.test/unsupported\""),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    assert_jsonapi_headers(&response);
    assert_eq!(
        serde_json::to_value(document(response).await).unwrap()["errors"][0]["code"],
        "not_acceptable"
    );
}

#[tokio::test]
async fn exact_zero_quality_overrides_wildcard_but_repeated_exact_ranges_are_combined() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter, true);
    let response = app
        .oneshot(request_with_accepts(
            "/ports",
            &["application/vnd.api+json;q=0", "*/*;q=1"],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    assert_jsonapi_headers(&response);

    let adapter = Arc::new(TestAdapter::default());
    *adapter.resource_result.lock().unwrap() = Some(port_record());
    let (app, _) = test_app(adapter, true);
    let response = app
        .oneshot(request_with_accepts(
            "/ports/1",
            &[
                "application/vnd.api+json;q=0",
                "application/vnd.api+json;q=1",
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
}

#[tokio::test]
async fn rejects_every_nonempty_query_string_before_adapter_calls() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter.clone(), true);

    let response = app
        .oneshot(request("/ports?&&page[number]=1", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "unsupported_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "page[number]");
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn adapter_failures_return_generic_jsonapi_errors_for_both_routes() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        if path == "/ports" {
            adapter.fail_collection.store(true, Ordering::SeqCst);
        } else {
            adapter.fail_resource.store(true, Ordering::SeqCst);
        }
        let (app, _) = test_app(adapter, true);

        let response = app.oneshot(request(path, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_jsonapi_headers(&response);
        let body = serde_json::to_value(document(response).await).unwrap();
        assert_eq!(body["errors"][0]["code"], "read_failed");
        assert_eq!(
            body["errors"][0]["detail"],
            "The resource could not be loaded."
        );
        assert!(
            !body["errors"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("must not leak")
        );
    }
}

#[tokio::test]
async fn rejects_relationship_linkage_to_an_unregistered_target_type() {
    let adapter = Arc::new(TestAdapter::default());
    let mut record = port_record();
    record.relationships.insert(
        "owner".to_owned(),
        Relationship {
            data: Some(RelationshipData::Many(vec![
                ResourceIdentifier {
                    type_name: "people".to_owned(),
                    id: Some("3".to_owned()),
                    ..ResourceIdentifier::default()
                },
                ResourceIdentifier {
                    type_name: "ships".to_owned(),
                    id: Some("9".to_owned()),
                    ..ResourceIdentifier::default()
                },
            ])),
            ..Relationship::default()
        },
    );
    *adapter.collection_result.lock().unwrap() = vec![record];
    let (app, _) = test_app(adapter, true);

    let response = app.oneshot(request("/ports", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_jsonapi_headers(&response);
    assert_eq!(
        serde_json::to_value(document(response).await).unwrap()["errors"][0]["code"],
        "read_failed"
    );
}
