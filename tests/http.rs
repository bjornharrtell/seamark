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
use seamark::http::{
    self, AdapterError, AdapterIncludedResource, AdapterResource, QueryAdapterError,
    QueryCollectionResult, QueryResourceAdapter, QueryResourceResult, RequestAuthorizer,
    ResourceAdapter,
};
use seamark::query::{
    FilterExpression, FilterValue, PaginationConfig, PlannedField, ReadPlan, SortDirection,
};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};
use tower::ServiceExt;

const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";
const QUERY_TEST_MAX_PAGE_SIZE: u64 = 100;

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

struct TestQueryAdapter {
    plans: Arc<Mutex<Vec<ReadPlan>>>,
    calls: AtomicUsize,
}

struct FailingQueryAdapter {
    calls: AtomicUsize,
}

#[async_trait]
impl QueryResourceAdapter for TestQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.plans.lock().unwrap().push(plan.clone());
        Ok(QueryCollectionResult {
            resources: vec![port_record()],
            included: vec![AdapterIncludedResource {
                resource_type: "people".to_owned(),
                resource: AdapterResource {
                    id: "3".to_owned(),
                    attributes: BTreeMap::from([("full_name".to_owned(), json!("Ada"))]),
                    ..AdapterResource::default()
                },
            }],
        })
    }

    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
        plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.plans.lock().unwrap().push(plan.clone());
        Ok(Some(QueryResourceResult {
            resource: port_record(),
            included: vec![AdapterIncludedResource {
                resource_type: "people".to_owned(),
                resource: AdapterResource {
                    id: "3".to_owned(),
                    attributes: BTreeMap::from([("full_name".to_owned(), json!("Ada"))]),
                    ..AdapterResource::default()
                },
            }],
        }))
    }
}

#[async_trait]
impl QueryResourceAdapter for FailingQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        _plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(QueryAdapterError::ReadFailed)
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

fn query_test_app(
    adapter: Arc<TestAdapter>,
    query_adapter: Arc<dyn QueryResourceAdapter>,
    allowed: bool,
) -> (Router, Arc<TestAuthorizer>) {
    let ports = ResourceDefinition::new("ports", "port_key")
        .attribute("name", "title", true, true)
        .attribute("depth", "depth_m", false, true)
        .relationship("owner", "owner", "people");
    let people =
        ResourceDefinition::new("people", "id").attribute("name", "full_name", false, false);
    let registry = Arc::new(ResourceRegistry::new([ports, people]).unwrap());
    let authorizer = Arc::new(TestAuthorizer {
        allowed,
        calls: AtomicUsize::new(0),
    });
    let pagination =
        PaginationConfig::new(1, 10, Some(QUERY_TEST_MAX_PAGE_SIZE), Some(1000)).unwrap();
    (
        http::router_with_query(
            registry,
            adapter,
            authorizer.clone(),
            query_adapter,
            pagination,
        ),
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

async fn error_document(response: Response<Body>) -> JsonApiDocument {
    let expected_status = response.status().as_u16().to_string();
    let document = document(response).await;
    let errors = document.errors.as_ref().expect("error response document");
    assert_eq!(errors.len(), 1);
    for error in errors {
        assert_eq!(error.status.as_deref(), Some(expected_status.as_str()));
    }
    document
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
async fn query_adapter_read_failures_map_to_matching_jsonapi_status() {
    let adapter = Arc::new(TestAdapter::default());
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let (app, _) = query_test_app(adapter, query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28name%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(body["data"][0]["id"], "1");
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);

    let failing_adapter = Arc::new(FailingQueryAdapter {
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter, failing_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28name%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "500");
    assert_eq!(body["errors"][0]["code"], "read_failed");
    assert_eq!(failing_adapter.calls.load(Ordering::SeqCst), 1);
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
    *adapter.resource_result.lock().unwrap() = Some(port_record());
    let (app, _) = test_app(adapter.clone(), true);

    let existing_response = app
        .clone()
        .oneshot(request("/ports/1", None))
        .await
        .unwrap();
    assert_eq!(existing_response.status(), StatusCode::OK);
    assert_jsonapi_headers(&existing_response);
    let existing_body = serde_json::to_value(document(existing_response).await).unwrap();
    assert_eq!(existing_body["data"]["id"], "1");

    *adapter.resource_result.lock().unwrap() = None;

    let response = app.oneshot(request("/ports/missing", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(
        body,
        json!({
            "errors": [{
                "status": "404",
                "code": "resource_not_found",
                "title": "Resource not found",
                "detail": "No `ports` resource has id `missing`."
            }]
        })
    );
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unknown_resource_type_returns_a_structured_error_without_adapter_calls() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter.clone(), true);

    let response = app.oneshot(request("/ships", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
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
        let body = serde_json::to_value(error_document(response).await).unwrap();
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
        let body = serde_json::to_value(error_document(response).await).unwrap();
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
                Some(
                    "application/vnd.api+json;profile=\"https://example.test/unknown https://example.test/also-unknown\"",
                ),
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
        serde_json::to_value(error_document(response).await).unwrap()["errors"][0]["code"],
        "not_acceptable"
    );
}

#[tokio::test]
async fn validates_profile_uri_lists_and_duplicate_accept_parameters() {
    for path in ["/ports", "/ports/1"] {
        for accept in [
            "application/vnd.api+json;profile=unquoted",
            "application/vnd.api+json;profile=\"relative/profile\"",
            "application/vnd.api+json;profile=\"https://example.test/one  https://example.test/two\"",
            "application/vnd.api+json;profile=\"https://example.test/one\";PROFILE=\"https://example.test/two\"",
        ] {
            let adapter = Arc::new(TestAdapter::default());
            let (app, _) = test_app(adapter, true);
            let response = app.oneshot(request(path, Some(accept))).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE, "{accept}");
            assert_jsonapi_headers(&response);
            error_document(response).await;
        }

        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter, true);
        let response = app
            .oneshot(request(
                path,
                Some(
                    "application/vnd.api+json;profile=\"https://example.test/one https://example.test/two\"",
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_jsonapi_headers(&response);
    }
}

#[tokio::test]
async fn ignores_accept_extensions_after_quality_and_unknown_media_parameters_match_no_range() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter, true);
        let response = app
            .oneshot(request(
                path,
                Some("application/vnd.api+json;q=0.5;profile=unquoted"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_jsonapi_headers(&response);
    }

    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter, true);
    let response = app
        .oneshot(request("/ports", Some("application/vnd.api+json;foo=bar")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    assert_jsonapi_headers(&response);
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
async fn accepts_only_spec_conformant_accept_quality_values_on_both_routes() {
    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        *adapter.resource_result.lock().unwrap() = Some(port_record());
        let (app, _) = test_app(adapter, true);
        let response = app
            .oneshot(request(path, Some("application/vnd.api+json;q=0.125")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_jsonapi_headers(&response);
    }

    for path in ["/ports", "/ports/1"] {
        let adapter = Arc::new(TestAdapter::default());
        let (app, _) = test_app(adapter.clone(), true);
        let response = app
            .oneshot(request(path, Some("application/vnd.api+json;q=0.1234")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
        assert_jsonapi_headers(&response);
        assert_eq!(
            serde_json::to_value(error_document(response).await).unwrap()["errors"][0]["code"],
            "not_acceptable"
        );
        assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn default_router_rejects_collection_and_resource_queries_before_adapter_calls() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = test_app(adapter.clone(), true);

    for (uri, parameter) in [
        ("/ports?&&page[number]=1", "page[number]"),
        ("/ports/1?include=owner", "include"),
    ] {
        let response = app.clone().oneshot(request(uri, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_jsonapi_headers(&response);
        let body = serde_json::to_value(error_document(response).await).unwrap();
        assert_eq!(body["errors"][0]["code"], "unsupported_query", "{uri}");
        assert_eq!(body["errors"][0]["source"]["parameter"], parameter, "{uri}");
    }

    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn requests_with_multiple_problems_return_one_first_error() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = test_app(adapter, true);

    let response = app
        .oneshot(request("/ships?unsupported=value", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let document = error_document(response).await;
    let errors = document.errors.unwrap();
    assert_eq!(errors[0].code.as_deref(), Some("unsupported_query"));
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
        let body = serde_json::to_value(error_document(response).await).unwrap();
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
        serde_json::to_value(error_document(response).await).unwrap()["errors"][0]["code"],
        "read_failed"
    );
}

#[tokio::test]
async fn query_router_plans_executes_and_projects_collection_queries() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28name%2C%27Harbor%27%29&sort=-depth&page%5Bnumber%5D=2&page%5Bsize%5D=5&fields%5Bports%5D=name,owner&include=owner",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
    let plan = plans.lock().unwrap()[0].clone();
    assert_eq!(
        plan.filter,
        Some(FilterExpression::Equals {
            model_field: "title".to_owned(),
            value: FilterValue::String("Harbor".to_owned()),
        })
    );
    assert_eq!(plan.sort[0].model_field, "depth_m");
    assert_eq!(plan.sort[0].direction, SortDirection::Descending);
    assert_eq!(plan.page.number, 2);
    assert_eq!(plan.page.size, 5);
    assert_eq!(plan.page.offset, 5);
    assert_eq!(
        plan.fieldsets["ports"],
        vec![
            PlannedField::Attribute {
                public_name: "name".to_owned(),
                model_field: "title".to_owned(),
            },
            PlannedField::Relationship {
                public_name: "owner".to_owned(),
                model_field: "owner".to_owned(),
                target_type: "people".to_owned(),
            },
        ]
    );
    assert_eq!(plan.includes[0].public_name, "owner");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["data"][0]["attributes"], json!({"name": null}));
    assert!(body["data"][0]["relationships"]["owner"].is_object());
    assert!(
        body["data"][0]["relationships"]
            .get("hidden_owner")
            .is_none()
    );
    assert_eq!(body["included"][0]["attributes"], json!({"name": "Ada"}));
}

#[tokio::test]
async fn query_router_plans_single_resource_includes_and_fieldsets() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports/1?include=owner&fields%5Bports%5D=name,owner&fields%5Bpeople%5D=name",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
    let plan = plans.lock().unwrap()[0].clone();
    assert!(plan.filter.is_none());
    assert!(plan.sort.is_empty());
    assert_eq!(plan.fieldsets["ports"].len(), 2);
    assert_eq!(plan.fieldsets["people"].len(), 1);
    assert_eq!(plan.includes[0].public_name, "owner");

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body,
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "attributes": {"name": null},
                "relationships": {"owner": {"data": {"type": "people", "id": "3"}}}
            },
            "included": [{
                "type": "people",
                "id": "3",
                "attributes": {"name": "Ada"}
            }]
        })
    );
}

#[tokio::test]
async fn query_adapter_without_single_resource_support_returns_not_implemented() {
    let query_adapter = Arc::new(FailingQueryAdapter {
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter, true);
    let response = app
        .oneshot(request("/ports/1?include=owner", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "resource_query_not_supported");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn single_resource_query_rejects_invalid_parameters_before_authorization_or_adapters() {
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);

    for (uri, parameter) in [
        ("/ports/1?filter=equals%28name%2C%27Alpha%27%29", "filter"),
        ("/ports/1?sort=name", "sort"),
        ("/ports/1?page%5Bnumber%5D=1", "page[number]"),
        ("/ports/1?page%5Bsize%5D=10", "page[size]"),
        ("/ports/1?fields%5Bports%5D=secret", "fields[ports]"),
        ("/ports/1?fields%5Bships%5D=name", "fields[ships]"),
        ("/ports/1?include=secret", "include"),
    ] {
        let response = app.clone().oneshot(request(uri, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_jsonapi_headers(&response);
        let body = serde_json::to_value(error_document(response).await).unwrap();
        assert_eq!(body["errors"][0]["code"], "invalid_query", "{uri}");
        assert_eq!(body["errors"][0]["source"]["parameter"], parameter, "{uri}");
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_invalid_queries_before_authorization_or_execution() {
    for (uri, parameter) in [
        ("/ports?unknown=value", "unknown"),
        ("/ports?include=%GG", "include"),
        ("/ports?page%5Bsize%5D=2&page%5Bsize%5D=3", "page[size]"),
    ] {
        let query_adapter = Arc::new(TestQueryAdapter {
            plans: Arc::new(Mutex::new(Vec::new())),
            calls: AtomicUsize::new(0),
        });
        let adapter = Arc::new(TestAdapter::default());
        let (app, authorizer) = query_test_app(adapter, query_adapter.clone(), true);
        let response = app.oneshot(request(uri, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0, "{uri}");
        assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0, "{uri}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["errors"][0]["code"], "invalid_query", "{uri}");
        assert_eq!(body["errors"][0]["source"]["parameter"], parameter, "{uri}");
    }
}

#[tokio::test]
async fn query_router_rejects_pagination_offset_overflow_before_authorization_or_execution() {
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?page%5Bnumber%5D=18446744073709551615&page%5Bsize%5D=2",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["detail"], "page offset overflows");
    assert_eq!(body["errors"][0]["source"]["parameter"], "page[number]");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_unsupported_filter_operator_before_execution() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28name%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        plans.lock().unwrap()[0].filter,
        Some(FilterExpression::Equals {
            model_field: "title".to_owned(),
            value: FilterValue::String("Harbor".to_owned()),
        })
    );

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=contains%28name%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "filter");
    assert_eq!(
        body["errors"][0]["detail"],
        "filter operator `contains` is not supported"
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_unknown_filter_field_before_authorization_or_execution() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28name%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        plans.lock().unwrap()[0].filter,
        Some(FilterExpression::Equals {
            model_field: "title".to_owned(),
            value: FilterValue::String("Harbor".to_owned()),
        })
    );

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?filter=equals%28secret%2C%27Harbor%27%29",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "filter");
    assert_eq!(
        body["errors"][0]["detail"],
        "attribute `secret` is not registered on resource `ports`"
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_unknown_sparse_field_before_execution() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?fields%5Bports%5D=name,owner&include=owner",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["data"][0]["attributes"], json!({"name": null}));
    assert!(body["data"][0]["relationships"]["owner"].is_object());
    assert_eq!(body["included"][0]["attributes"], json!({"name": "Ada"}));
    assert_eq!(
        plans.lock().unwrap()[0].fieldsets["ports"],
        vec![
            PlannedField::Attribute {
                public_name: "name".to_owned(),
                model_field: "title".to_owned(),
            },
            PlannedField::Relationship {
                public_name: "owner".to_owned(),
                model_field: "owner".to_owned(),
                target_type: "people".to_owned(),
            },
        ]
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request(
            "/ports?fields%5Bports%5D=secret&include=owner",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "fields[ports]");
    assert_eq!(
        body["errors"][0]["detail"],
        "fieldset field `secret` is not registered on resource `ports`"
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_unknown_sort_field_before_execution() {
    let plans = Arc::new(Mutex::new(Vec::new()));
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: plans.clone(),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request("/ports?sort=depth", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let plan = plans.lock().unwrap()[0].clone();
    assert_eq!(plan.sort[0].public_name, "depth");
    assert_eq!(plan.sort[0].model_field, "depth_m");
    assert_eq!(plan.sort[0].direction, SortDirection::Ascending);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request("/ports?sort=secret", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["source"]["parameter"], "sort");
    assert_eq!(
        body["errors"][0]["detail"],
        "sort attribute `secret` is not registered on resource `ports`"
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_enforces_configured_page_size_limit_before_execution() {
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let at_limit_uri = format!("/ports?page%5Bsize%5D={QUERY_TEST_MAX_PAGE_SIZE}");
    let response = app.oneshot(request(&at_limit_uri, None)).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let over_limit_uri = format!("/ports?page%5Bsize%5D={}", QUERY_TEST_MAX_PAGE_SIZE + 1);
    let response = app.oneshot(request(&over_limit_uri, None)).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["source"]["parameter"], "page[size]");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_rejects_unknown_include_relationship_before_execution() {
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request("/ports?include=owner", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(
        body["included"],
        json!([{
            "type": "people",
            "id": "3",
            "attributes": {"name": "Ada"}
        }])
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), true);
    let response = app
        .oneshot(request("/ports?include=owner.unknown", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "invalid_query");
    assert_eq!(body["errors"][0]["status"], "400");
    assert_eq!(body["errors"][0]["source"]["parameter"], "include");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn query_router_authorizes_before_calling_query_adapter() {
    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter, query_adapter.clone(), false);
    let response = app
        .oneshot(request("/ports?sort=name", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
}
