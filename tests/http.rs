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
    self, AdapterError, AdapterIncludedResource, AdapterResource, MutationAdapterError,
    MutationCommand, MutationOutcome, MutationResourceAdapter, QueryAdapterError,
    QueryCollectionResult, QueryResourceAdapter, QueryResourceResult, RelationshipMutation,
    RequestAuthorizer, ResourceAdapter, ResourceMutationChangeset,
};
use seamark::query::{
    FilterExpression, FilterValue, PaginationConfig, PlannedField, ReadPlan, SortDirection,
};
use seamark::registry::{
    RelationshipCardinality, RelationshipMapping, ResourceDefinition, ResourceRegistry,
};
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

struct EmptyIncludeQueryAdapter;

#[async_trait]
impl QueryResourceAdapter for EmptyIncludeQueryAdapter {
    async fn collection(
        &self,
        _resource: &ResourceDefinition,
        _plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError> {
        Ok(QueryCollectionResult {
            resources: vec![port_record_without_owner()],
            included: Vec::new(),
        })
    }

    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
        _plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        Ok(Some(QueryResourceResult {
            resource: port_record_without_owner(),
            included: Vec::new(),
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

#[derive(Default)]
struct TestMutationAdapter {
    commands: Mutex<Vec<MutationCommand>>,
    failure: Mutex<Option<MutationAdapterError>>,
}

#[async_trait]
impl MutationResourceAdapter for TestMutationAdapter {
    async fn execute(
        &self,
        resource: &ResourceDefinition,
        command: MutationCommand,
    ) -> Result<MutationOutcome, MutationAdapterError> {
        self.commands.lock().unwrap().push(command.clone());
        if let Some(error) = *self.failure.lock().unwrap() {
            return Err(error);
        }
        match command {
            MutationCommand::Create { changeset } => Ok(MutationOutcome::Resource(
                record_from_changeset(resource, "42", changeset),
            )),
            MutationCommand::Update { id, changeset } => Ok(MutationOutcome::Resource(
                record_from_changeset(resource, &id, changeset),
            )),
            MutationCommand::Delete { .. } => Ok(MutationOutcome::Deleted),
            MutationCommand::ReadRelationship { relationship, .. } => {
                Ok(MutationOutcome::Relationship(empty_linkage(&relationship)))
            }
            MutationCommand::ModifyRelationship {
                relationship,
                mutation,
                ..
            } => {
                let data = match mutation {
                    RelationshipMutation::Replace(data) => data,
                    RelationshipMutation::Add(identifiers)
                    | RelationshipMutation::Remove(identifiers) => {
                        seamark::document::RelationshipData::Many(identifiers)
                    }
                };
                if !linkage_matches_test_mapping(&data, &relationship) {
                    return Err(MutationAdapterError::Failed);
                }
                Ok(MutationOutcome::Relationship(data))
            }
        }
    }
}

fn record_from_changeset(
    _definition: &ResourceDefinition,
    id: &str,
    changeset: ResourceMutationChangeset,
) -> AdapterResource {
    AdapterResource {
        id: id.to_owned(),
        attributes: changeset.attributes,
        relationships: changeset
            .relationships
            .into_iter()
            .map(|(field, data)| {
                (
                    field,
                    seamark::document::Relationship {
                        data: Some(data),
                        ..Default::default()
                    },
                )
            })
            .collect(),
    }
}

fn empty_linkage(relationship: &RelationshipMapping) -> seamark::document::RelationshipData {
    match relationship.cardinality() {
        Some(RelationshipCardinality::ToOne) => seamark::document::RelationshipData::Null,
        Some(RelationshipCardinality::ToMany) => {
            seamark::document::RelationshipData::Many(Vec::new())
        }
        None => unreachable!("mutation routes require declared cardinality"),
    }
}

fn linkage_matches_test_mapping(
    data: &seamark::document::RelationshipData,
    relationship: &RelationshipMapping,
) -> bool {
    matches!(
        (relationship.cardinality(), data),
        (
            Some(RelationshipCardinality::ToOne),
            seamark::document::RelationshipData::Null | seamark::document::RelationshipData::One(_)
        ) | (
            Some(RelationshipCardinality::ToMany),
            seamark::document::RelationshipData::Many(_)
        )
    )
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

fn port_record_without_owner() -> AdapterResource {
    let mut record = port_record();
    record.relationships.get_mut("owner").unwrap().data = Some(RelationshipData::Null);
    record
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

fn mutation_test_app(allowed: bool) -> (Router, Arc<TestMutationAdapter>, Arc<TestAuthorizer>) {
    let ports = ResourceDefinition::new("ports", "port_id")
        .attribute("name", "title", false, false)
        .to_one_relationship("owner", "owner_id", "people")
        .to_many_relationship("tags", "tag_links", "tags");
    let people = ResourceDefinition::new("people", "person_id");
    let tags = ResourceDefinition::new("tags", "tag_id");
    let registry = Arc::new(ResourceRegistry::new([ports, people, tags]).unwrap());
    let authorizer = Arc::new(TestAuthorizer {
        allowed,
        calls: AtomicUsize::new(0),
    });
    let mutation_adapter = Arc::new(TestMutationAdapter::default());
    let app = http::router_with_mutations(
        registry,
        Arc::new(TestAdapter::default()),
        authorizer.clone(),
        mutation_adapter.clone(),
    );
    (app, mutation_adapter, authorizer)
}

fn request(uri: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(accept) = accept {
        builder = builder.header(ACCEPT, accept);
    }
    builder.body(Body::empty()).unwrap()
}

fn mutation_request(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(ACCEPT, JSONAPI_MEDIA_TYPE)
        .header(CONTENT_TYPE, JSONAPI_MEDIA_TYPE)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn read_request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(ACCEPT, JSONAPI_MEDIA_TYPE)
        .body(Body::empty())
        .unwrap()
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
async fn base_resource_create_returns_created_representation_and_mapped_changeset() {
    let (app, adapter, _) = mutation_test_app(true);
    let response = app
        .oneshot(mutation_request(
            "POST",
            "/ports",
            r#"{"data":{"type":"ports","attributes":{"name":"West"},"relationships":{"owner":{"data":{"type":"people","id":"7"}}}}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_jsonapi_headers(&response);
    assert_eq!(response.headers().get("location").unwrap(), "/ports/42");
    let document = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(
        document,
        json!({
            "data": {
                "type": "ports",
                "id": "42",
                "attributes": {"name": "West"},
                "relationships": {"owner": {"data": {"type": "people", "id": "7"}}}
            }
        })
    );
    let commands = adapter.commands.lock().unwrap();
    let MutationCommand::Create { changeset } = &commands[0] else {
        panic!("expected a resource create command");
    };
    assert_eq!(changeset.attributes.get("title"), Some(&json!("West")));
    assert!(!changeset.attributes.contains_key("name"));
    assert_eq!(
        changeset.relationships.get("owner_id"),
        Some(&seamark::document::RelationshipData::One(
            seamark::document::ResourceIdentifier {
                type_name: "people".to_owned(),
                id: Some("7".to_owned()),
                ..Default::default()
            }
        ))
    );
}

#[tokio::test]
async fn base_resource_patch_preserves_omitted_fields_and_explicit_null() {
    let (app, adapter, _) = mutation_test_app(true);
    let response = app
        .oneshot(mutation_request(
            "PATCH",
            "/ports/3",
            r#"{"data":{"type":"ports","id":"3","attributes":{"name":null}}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    let document = serde_json::to_value(document(response).await).unwrap();
    assert_eq!(document["data"]["id"], "3");
    assert_eq!(document["data"]["attributes"]["name"], Value::Null);
    let commands = adapter.commands.lock().unwrap();
    let MutationCommand::Update { id, changeset } = &commands[0] else {
        panic!("expected a resource update command");
    };
    assert_eq!(id, "3");
    assert_eq!(changeset.attributes.get("title"), Some(&Value::Null));
    assert!(changeset.relationships.is_empty());
}

#[tokio::test]
async fn base_resource_delete_returns_no_content() {
    let (app, adapter, _) = mutation_test_app(true);
    let request = Request::builder()
        .method("DELETE")
        .uri("/ports/3")
        .header(ACCEPT, JSONAPI_MEDIA_TYPE)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get(CONTENT_TYPE).is_none());
    assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap().len(), 0);
    assert!(matches!(
        adapter.commands.lock().unwrap().as_slice(),
        [MutationCommand::Delete { id }] if id == "3"
    ));
}

#[tokio::test]
async fn relationship_linkage_routes_read_replace_add_and_remove() {
    let (app, adapter, _) = mutation_test_app(true);
    let response = app
        .clone()
        .oneshot(read_request("/ports/1/relationships/tags"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &to_bytes(response.into_body(), 1024 * 1024).await.unwrap()
        )
        .unwrap(),
        json!({"data": []})
    );

    let response = app
        .clone()
        .oneshot(mutation_request(
            "PATCH",
            "/ports/1/relationships/owner",
            r#"{"data":{"type":"people","id":"9"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &to_bytes(response.into_body(), 1024 * 1024).await.unwrap()
        )
        .unwrap(),
        json!({"data": {"type": "people", "id": "9"}})
    );

    for (method, identifier) in [("POST", "4"), ("DELETE", "5")] {
        let response = app
            .clone()
            .oneshot(mutation_request(
                method,
                "/ports/1/relationships/tags",
                &format!(r#"{{"data":[{{"type":"tags","id":"{identifier}"}}]}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = serde_json::from_slice::<Value>(
            &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(value["data"][0]["id"], identifier);
    }
    let commands = adapter.commands.lock().unwrap();
    assert!(matches!(
        commands.as_slice(),
        [
            MutationCommand::ReadRelationship { .. },
            MutationCommand::ModifyRelationship {
                mutation: RelationshipMutation::Replace(_),
                ..
            },
            MutationCommand::ModifyRelationship {
                mutation: RelationshipMutation::Add(_),
                ..
            },
            MutationCommand::ModifyRelationship {
                mutation: RelationshipMutation::Remove(_),
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn registered_base_routes_return_jsonapi_errors_for_unsupported_methods() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    for uri in ["/ports", "/ports/1", "/ports/1/relationships/tags"] {
        let response = app
            .clone()
            .oneshot(mutation_request("PUT", uri, ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        assert_jsonapi_headers(&response);
        assert!(response.headers().contains_key("allow"));
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("method_not_allowed"));
        assert_eq!(errors[0].status.as_deref(), Some("405"));
    }
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn opt_in_jsonapi_not_found_fallback_composes_with_application_routes() {
    let (api, adapter, authorizer) = mutation_test_app(true);
    let app = Router::new()
        .route(
            "/health",
            axum::routing::get(|| async { StatusCode::NO_CONTENT }),
        )
        .merge(api)
        .fallback(http::not_found_fallback);

    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::NO_CONTENT);

    let response = app
        .oneshot(read_request("/unregistered/path/extra"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_jsonapi_headers(&response);
    let errors = error_document(response).await.errors.unwrap();
    assert_eq!(errors[0].code.as_deref(), Some("route_not_found"));
    assert_eq!(errors[0].status.as_deref(), Some("404"));
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mutation_validation_and_authorization_precede_adapter_execution() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    let response = app
        .clone()
        .oneshot(mutation_request(
            "POST",
            "/ports",
            r#"{"data":{"type":"ports","attributes":{"secret":"hidden"}}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let document = error_document(response).await;
    assert_eq!(
        document.errors.as_ref().unwrap()[0]
            .source
            .as_ref()
            .unwrap()
            .pointer
            .as_deref(),
        Some("/data/attributes/secret")
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());

    let mut invalid_media_type = mutation_request("POST", "/ports", r#"{"data":{"type":"ports"}}"#);
    invalid_media_type
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let response = app.oneshot(invalid_media_type).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());

    let (app, adapter, authorizer) = mutation_test_app(false);
    let response = app
        .oneshot(mutation_request("DELETE", "/ports/1", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn base_mutation_routes_reject_duplicate_json_members_before_authorization_or_adapter() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    for (method, uri, body) in [
        (
            "POST",
            "/ports",
            r#"{"data":{"type":"ports"},"data":{"type":"ports"}}"#,
        ),
        (
            "POST",
            "/ports",
            r#"{"data":{"type":"ports","attributes":{"name":"first","name":"second"}}}"#,
        ),
        (
            "PATCH",
            "/ports/1/relationships/owner",
            r#"{"data":{"type":"people","id":"1","id":"2"}}"#,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(mutation_request(method, uri, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("invalid_document"));
        assert_eq!(errors[0].status.as_deref(), Some("400"));
        assert!(errors[0].source.is_none());
        assert!(
            errors[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("duplicate JSON object member"))
        );
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_base_mutation_json_omits_source_pointer_before_authorization_or_adapter() {
    let (app, adapter, authorizer) = mutation_test_app(false);
    for (method, uri, body) in [
        ("POST", "/ports", "{not-json"),
        ("PATCH", "/ports/1/relationships/owner", "{"),
    ] {
        let response = app
            .clone()
            .oneshot(mutation_request(method, uri, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("invalid_document"));
        assert_eq!(errors[0].status.as_deref(), Some("400"));
        assert!(errors[0].source.is_none());
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn typed_invalid_mutation_document_omits_source_pointer_before_authorization_or_adapter() {
    let (app, adapter, authorizer) = mutation_test_app(false);
    let response = app
        .oneshot(mutation_request("POST", "/ports", r#"{"jsonapi":false}"#))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_jsonapi_headers(&response);
    let errors = error_document(response).await.errors.unwrap();
    assert_eq!(errors[0].code.as_deref(), Some("invalid_document"));
    assert_eq!(errors[0].status.as_deref(), Some("400"));
    assert!(
        errors[0]
            .source
            .as_ref()
            .and_then(|source| source.pointer.as_deref())
            .is_none()
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn base_mutation_routes_reject_query_parameters_before_authorization_or_adapter() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    let cases = [
        (
            "POST",
            "/ports?fields[ports]=name",
            r#"{"data":{"type":"ports"}}"#,
        ),
        (
            "PATCH",
            "/ports/1?fields[ports]=name",
            r#"{"data":{"type":"ports","id":"1"}}"#,
        ),
        ("DELETE", "/ports/1?fields[ports]=name", ""),
        ("GET", "/ports/1/relationships/owner?fields[ports]=name", ""),
        (
            "PATCH",
            "/ports/1/relationships/owner?fields[ports]=name",
            r#"{"data":null}"#,
        ),
        (
            "POST",
            "/ports/1/relationships/tags?fields[ports]=name",
            r#"{"data":[{"type":"tags","id":"2"}]}"#,
        ),
        (
            "DELETE",
            "/ports/1/relationships/tags?fields[ports]=name",
            r#"{"data":[{"type":"tags","id":"2"}]}"#,
        ),
    ];

    for (method, uri, body) in cases {
        let response = app
            .clone()
            .oneshot(mutation_request(method, uri, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("invalid_query"));
        assert_eq!(errors[0].status.as_deref(), Some("400"));
        assert_eq!(
            errors[0].source.as_ref().unwrap().parameter.as_deref(),
            Some("fields[ports]")
        );
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn base_mutation_content_type_is_validated_before_authorization_or_adapter() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    let invalid_content_types = [
        None,
        Some("application/json"),
        Some("application/vnd.api+json; charset=utf-8"),
        Some("application/vnd.api+json; ext=\"https://jsonapi.org/ext/atomic\""),
        Some("application/vnd.api+json; profile=https://example.com/profile"),
        Some("application/vnd.api+json; profile=\"\""),
        Some("application/vnd.api+json; profile=\"relative\""),
        Some(
            "application/vnd.api+json; profile=\"https://example.com/one  https://example.com/two\"",
        ),
        Some(
            "application/vnd.api+json; profile=\"https://example.com/one\"; profile=\"https://example.com/two\"",
        ),
    ];

    for content_type in invalid_content_types {
        let mut request = mutation_request("POST", "/ports", r#"{"data":{"type":"ports"}}"#);
        if let Some(content_type) = content_type {
            request
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
        } else {
            request.headers_mut().remove(CONTENT_TYPE);
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("unsupported_media_type"));
        assert_eq!(errors[0].status.as_deref(), Some("415"));
    }

    let mut duplicate_content_type =
        mutation_request("POST", "/ports", r#"{"data":{"type":"ports"}}"#);
    duplicate_content_type
        .headers_mut()
        .append(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let response = app.clone().oneshot(duplicate_content_type).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_jsonapi_headers(&response);
    let errors = error_document(response).await.errors.unwrap();
    assert_eq!(errors[0].code.as_deref(), Some("unsupported_media_type"));
    assert_eq!(errors[0].status.as_deref(), Some("415"));

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());

    let (app, adapter, authorizer) = mutation_test_app(false);
    let mut valid_profile = mutation_request("POST", "/ports", r#"{"data":{"type":"ports"}}"#);
    valid_profile.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(
            "APPLICATION/VND.API+JSON;PROFILE=\"https://example.com/profile;version=1\"",
        ),
    );
    let response = app.clone().oneshot(valid_profile).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert!(adapter.commands.lock().unwrap().is_empty());

    let mut multiple_profiles = mutation_request("POST", "/ports", r#"{"data":{"type":"ports"}}"#);
    multiple_profiles.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(
            "application/vnd.api+json;profile=\"https://example.com/one https://example.com/two\"",
        ),
    );
    let response = app.oneshot(multiple_profiles).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 2);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bodyless_routes_reject_unsupported_jsonapi_content_type_parameters_before_execution() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = test_app(adapter.clone(), true);

    for uri in ["/ports", "/ports/1"] {
        let mut request = read_request(uri);
        request.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.api+json;charset=utf-8"),
        );
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{uri}"
        );
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("unsupported_media_type"));
    }
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);

    let mut request = read_request("/ports");
    request.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(
            "application/vnd.api+json;profile=\"https://example.test/profile\"",
        ),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_jsonapi_headers(&response);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 1);

    let (app, adapter, authorizer) = mutation_test_app(true);
    let mut request = read_request("/ports/42/relationships/owner");
    request.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.api+json;charset=utf-8"),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_jsonapi_headers(&response);
    let errors = error_document(response).await.errors.unwrap();
    assert_eq!(errors[0].code.as_deref(), Some("unsupported_media_type"));
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn base_mutation_and_relationship_routes_reject_unacceptable_accept_before_execution() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    let requests = [
        (
            "POST",
            "/ports",
            r#"{"data":{"type":"ports","attributes":{"name":"West"}}}"#,
        ),
        (
            "PATCH",
            "/ports/1",
            r#"{"data":{"type":"ports","id":"1","attributes":{"name":"West"}}}"#,
        ),
        ("DELETE", "/ports/1", ""),
        ("GET", "/ports/1/relationships/tags", ""),
        (
            "PATCH",
            "/ports/1/relationships/owner",
            r#"{"data":{"type":"people","id":"1"}}"#,
        ),
        ("POST", "/ports/1/relationships/tags", r#"{"data":[]}"#),
        ("DELETE", "/ports/1/relationships/tags", r#"{"data":[]}"#),
    ];

    for (method, uri, body) in requests {
        let mut request = mutation_request(method, uri, body);
        request
            .headers_mut()
            .insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = app.clone().oneshot(request).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::NOT_ACCEPTABLE,
            "{method} {uri}"
        );
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some("not_acceptable"));
        assert_eq!(errors[0].status.as_deref(), Some("406"));
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn base_mutation_adapter_errors_map_to_jsonapi_http_statuses() {
    let cases = [
        (
            MutationAdapterError::NotFound,
            StatusCode::NOT_FOUND,
            "resource_not_found",
        ),
        (
            MutationAdapterError::RelatedResourceNotFound,
            StatusCode::NOT_FOUND,
            "related_resource_not_found",
        ),
        (
            MutationAdapterError::Conflict,
            StatusCode::CONFLICT,
            "resource_conflict",
        ),
        (
            MutationAdapterError::Unsupported,
            StatusCode::FORBIDDEN,
            "unsupported_operation",
        ),
        (
            MutationAdapterError::Failed,
            StatusCode::INTERNAL_SERVER_ERROR,
            "mutation_failed",
        ),
    ];

    for (adapter_error, expected_status, expected_code) in cases {
        let (app, adapter, authorizer) = mutation_test_app(true);
        *adapter.failure.lock().unwrap() = Some(adapter_error);
        let response = app
            .oneshot(mutation_request("DELETE", "/ports/1", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
        assert_jsonapi_headers(&response);
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some(expected_code));
        assert_eq!(errors[0].status.as_deref(), Some(expected_status.as_str()));
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            adapter.commands.lock().unwrap().as_slice(),
            [MutationCommand::Delete { id }] if id == "1"
        ));
    }
}

#[tokio::test]
async fn base_mutation_statuses_follow_resource_identity_and_linkage_rules() {
    let (app, adapter, authorizer) = mutation_test_app(true);
    let cases = [
        (
            "POST",
            "/ports",
            r#"{"data":{"type":"ports","id":"client-id"}}"#,
            StatusCode::FORBIDDEN,
            "client_generated_id_not_supported",
        ),
        (
            "POST",
            "/ports",
            r#"{"data":{"type":"people"}}"#,
            StatusCode::CONFLICT,
            "resource_type_mismatch",
        ),
        (
            "PATCH",
            "/ports/1",
            r#"{"data":{"type":"people","id":"1"}}"#,
            StatusCode::CONFLICT,
            "resource_type_mismatch",
        ),
        (
            "PATCH",
            "/ports/1",
            r#"{"data":{"type":"ports","id":"2"}}"#,
            StatusCode::CONFLICT,
            "resource_id_mismatch",
        ),
        (
            "PATCH",
            "/ports/1",
            r#"{"data":{"type":"ports","id":"1","relationships":{"owner":{}}}}"#,
            StatusCode::BAD_REQUEST,
            "relationship_data_required",
        ),
    ];
    for (method, uri, body, expected_status, expected_code) in cases {
        let response = app
            .clone()
            .oneshot(mutation_request(method, uri, body))
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status, "{method} {uri}");
        let errors = error_document(response).await.errors.unwrap();
        assert_eq!(errors[0].code.as_deref(), Some(expected_code));
    }
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());

    let response = app
        .oneshot(mutation_request(
            "POST",
            "/ports/1/relationships/owner",
            r#"{"data":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn required_mutation_data_error_pointers_resolve_in_the_request_document() {
    let (app, adapter, authorizer) = mutation_test_app(false);
    let cases = [
        ("POST", "/ports", "{}", ""),
        ("POST", "/ports", r#"{"errors":[]}"#, "/errors"),
        ("POST", "/ports", r#"{"included":[]}"#, "/included"),
        (
            "PATCH",
            "/ports/1",
            r#"{"data":{"type":"ports","id":"1","relationships":{"owner":{}}}}"#,
            "/data/relationships/owner",
        ),
        ("PATCH", "/ports/1/relationships/owner", "{}", ""),
        ("PATCH", "/ports/1/relationships/owner", "null", ""),
    ];

    for (method, uri, body, expected_pointer) in cases {
        let request_document: Value = serde_json::from_str(body).unwrap();
        let response = app
            .clone()
            .oneshot(mutation_request(method, uri, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        let errors = error_document(response).await.errors.unwrap();
        let pointer = errors[0]
            .source
            .as_ref()
            .and_then(|source| source.pointer.as_deref())
            .expect("request error source pointer");

        assert_eq!(pointer, expected_pointer, "{method} {uri}");
        assert!(
            request_document.pointer(pointer).is_some(),
            "{method} {uri} pointer {pointer:?} must resolve in the request document"
        );
    }

    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.commands.lock().unwrap().is_empty());
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
        for accept in [
            "application/vnd.api+json;q=0.125",
            "application/vnd.api+json;q=1.",
            "application/vnd.api+json;q=1;foo",
            r#"application/vnd.api+json;q=1;foo="x\"y""#,
        ] {
            let adapter = Arc::new(TestAdapter::default());
            *adapter.resource_result.lock().unwrap() = Some(port_record());
            let (app, _) = test_app(adapter, true);
            let response = app.oneshot(request(path, Some(accept))).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}: {accept}");
            assert_jsonapi_headers(&response);
        }
    }

    for path in ["/ports", "/ports/1"] {
        for accept in [
            "application/vnd.api+json;q=0.1234",
            r#"application/vnd.api+json;q=1;foo="x\""#,
        ] {
            let adapter = Arc::new(TestAdapter::default());
            let (app, authorizer) = test_app(adapter.clone(), true);
            let response = app.oneshot(request(path, Some(accept))).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_ACCEPTABLE,
                "{path}: {accept}"
            );
            assert_jsonapi_headers(&response);
            assert_eq!(
                serde_json::to_value(error_document(response).await).unwrap()["errors"][0]["code"],
                "not_acceptable"
            );
            assert_eq!(authorizer.calls.load(Ordering::SeqCst), 0);
            assert_eq!(adapter.collection_calls.load(Ordering::SeqCst), 0);
            assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
        }
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
async fn query_router_returns_empty_included_for_requested_includes_without_targets() {
    let adapter = Arc::new(TestAdapter::default());
    let (app, _) = query_test_app(adapter, Arc::new(EmptyIncludeQueryAdapter), true);

    for uri in [
        "/ports?include=owner",
        "/ports/1?include=owner",
        "/ports?include=",
        "/ports/1?include=",
    ] {
        let response = app.clone().oneshot(request(uri, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_jsonapi_headers(&response);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["included"], json!([]), "{uri}");
        let root = if uri.starts_with("/ports/1") {
            &body["data"]
        } else {
            &body["data"][0]
        };
        assert_eq!(root["relationships"]["owner"]["data"], Value::Null, "{uri}");
    }
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

    let query_adapter = Arc::new(TestQueryAdapter {
        plans: Arc::new(Mutex::new(Vec::new())),
        calls: AtomicUsize::new(0),
    });
    let adapter = Arc::new(TestAdapter::default());
    let (app, authorizer) = query_test_app(adapter.clone(), query_adapter.clone(), false);
    let response = app
        .oneshot(request(
            "/ports/1?include=owner&fields%5Bports%5D=name,owner&fields%5Bpeople%5D=name",
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_jsonapi_headers(&response);
    let body = serde_json::to_value(error_document(response).await).unwrap();
    assert_eq!(body["errors"][0]["code"], "forbidden");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(query_adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.resource_calls.load(Ordering::SeqCst), 0);
}
