//! Axum HTTP integration for the initial read-only vertical slice.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Map, Value};

use crate::document::{
    ErrorObject, ErrorSource, JsonApiDocument, PrimaryData, Relationship, RelationshipData,
    ResourceObject,
};
use crate::registry::{ResourceDefinition, ResourceRegistry};

const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";

/// A resource record returned by a persistence adapter.
///
/// Attribute and relationship map keys are internal field names. The HTTP
/// layer projects only fields declared in the public resource registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdapterResource {
    /// A normalized persistent identifier for the public JSON:API resource.
    ///
    /// The adapter resolves the registered
    /// [`ResourceDefinition::identifier_field`] and returns its value in this
    /// representation; the HTTP layer does not interpret raw model fields.
    pub id: String,
    /// Attribute values keyed by internal field name.
    pub attributes: BTreeMap<String, Value>,
    /// Relationship objects keyed by internal field name.
    pub relationships: BTreeMap<String, Relationship>,
}

/// An adapter read failed.
///
/// Adapter-specific failure details are intentionally not exposed in HTTP
/// error documents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterError;

/// The asynchronous read operations required by the initial GET routes.
#[async_trait]
pub trait ResourceAdapter: Send + Sync + 'static {
    /// Reads a resource collection.
    async fn collection(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<Vec<AdapterResource>, AdapterError>;

    /// Reads one resource by persistent identifier.
    async fn resource(
        &self,
        resource: &ResourceDefinition,
        id: &str,
    ) -> Result<Option<AdapterResource>, AdapterError>;
}

/// Authorizes access before a persistence adapter is called.
#[async_trait]
pub trait RequestAuthorizer: Send + Sync + 'static {
    /// Returns whether the request may read the resource or collection.
    async fn authorize(
        &self,
        resource_type: &str,
        resource_id: Option<&str>,
        headers: &HeaderMap,
    ) -> bool;
}

/// An authorizer that permits all reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAuthorizer;

#[async_trait]
impl RequestAuthorizer for AllowAllAuthorizer {
    async fn authorize(
        &self,
        _resource_type: &str,
        _resource_id: Option<&str>,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }
}

struct ApiState {
    registry: Arc<ResourceRegistry>,
    adapter: Arc<dyn ResourceAdapter>,
    authorizer: Arc<dyn RequestAuthorizer>,
}

/// Builds the collection and single-resource GET routes.
///
/// The routes reject all non-empty query strings. Filtering, sorting,
/// pagination, includes, writes, and persistence implementations are outside
/// this adapter boundary.
pub fn router(
    registry: Arc<ResourceRegistry>,
    adapter: Arc<dyn ResourceAdapter>,
    authorizer: Arc<dyn RequestAuthorizer>,
) -> Router {
    let state = Arc::new(ApiState {
        registry,
        adapter,
        authorizer,
    });
    Router::new()
        .route("/{resource_type}", get(get_collection))
        .route("/{resource_type}/{id}", get(get_resource))
        .with_state(state)
}

async fn get_collection(
    State(state): State<Arc<ApiState>>,
    Path(resource_type): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = validate_request(&headers, query.as_deref()) {
        return request_error_response(error);
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => {
            return protocol_error(
                StatusCode::NOT_FOUND,
                "unknown_resource_type",
                "Resource type not found",
                Some(format!(
                    "Resource type `{resource_type}` is not registered."
                )),
                None,
            );
        }
    };
    if !state
        .authorizer
        .authorize(&resource_type, None, &headers)
        .await
    {
        return forbidden_error();
    }

    let records = match state.adapter.collection(definition).await {
        Ok(records) => records,
        Err(_) => return adapter_error(),
    };
    let resources = match records
        .iter()
        .map(|record| project_resource(definition, record))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(resources) => resources,
        Err(_) => return adapter_error(),
    };
    respond_with_document(
        StatusCode::OK,
        JsonApiDocument {
            data: Some(PrimaryData::Many(resources)),
            ..JsonApiDocument::default()
        },
    )
}

async fn get_resource(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = validate_request(&headers, query.as_deref()) {
        return request_error_response(error);
    }
    let definition = match state.registry.resource(&resource_type) {
        Ok(definition) => definition,
        Err(_) => {
            return protocol_error(
                StatusCode::NOT_FOUND,
                "unknown_resource_type",
                "Resource type not found",
                Some(format!(
                    "Resource type `{resource_type}` is not registered."
                )),
                None,
            );
        }
    };
    if !state
        .authorizer
        .authorize(&resource_type, Some(&id), &headers)
        .await
    {
        return forbidden_error();
    }

    let record = match state.adapter.resource(definition, &id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return protocol_error(
                StatusCode::NOT_FOUND,
                "resource_not_found",
                "Resource not found",
                Some(format!("No `{resource_type}` resource has id `{id}`.")),
                None,
            );
        }
        Err(_) => return adapter_error(),
    };
    let resource = match project_resource(definition, &record) {
        Ok(resource) => resource,
        Err(_) => return adapter_error(),
    };
    respond_with_document(
        StatusCode::OK,
        JsonApiDocument {
            data: Some(PrimaryData::One(resource)),
            ..JsonApiDocument::default()
        },
    )
}

enum RequestValidationError {
    NotAcceptable,
    UnsupportedQuery(Option<String>),
}

fn validate_request(
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<(), RequestValidationError> {
    if !accepts_jsonapi(headers) {
        return Err(RequestValidationError::NotAcceptable);
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let parameter = first_query_parameter(query);
        return Err(RequestValidationError::UnsupportedQuery(parameter));
    }
    Ok(())
}

fn request_error_response(error: RequestValidationError) -> Response {
    match error {
        RequestValidationError::NotAcceptable => protocol_error(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            "JSON:API response is not acceptable",
            Some(format!(
                "This endpoint only produces `{JSONAPI_MEDIA_TYPE}`."
            )),
            None,
        ),
        RequestValidationError::UnsupportedQuery(parameter) => protocol_error(
            StatusCode::BAD_REQUEST,
            "unsupported_query",
            "Unsupported query parameter",
            Some("This endpoint does not support query parameters.".to_owned()),
            parameter,
        ),
    }
}

fn first_query_parameter(query: &str) -> Option<String> {
    query
        .split('&')
        .filter(|component| !component.is_empty())
        .map(|component| component.split('=').next().unwrap_or_default())
        .find(|name| !name.is_empty())
        .map(str::to_owned)
}

fn accepts_jsonapi(headers: &HeaderMap) -> bool {
    let values = headers.get_all(ACCEPT);
    if values.iter().next().is_none() {
        return true;
    }

    let mut best_match: Option<(u8, f32)> = None;
    for value in values {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for range in split_quoted(value, ',') {
            if let Some((specificity, quality)) = parse_media_range(range) {
                match best_match {
                    Some((best_specificity, _)) if best_specificity > specificity => {}
                    Some((best_specificity, best_quality))
                        if best_specificity == specificity && best_quality >= quality => {}
                    _ => best_match = Some((specificity, quality)),
                }
            }
        }
    }
    best_match.is_some_and(|(_, quality)| quality > 0.0)
}

fn parse_media_range(range: &str) -> Option<(u8, f32)> {
    let mut segments = split_quoted(range, ';').into_iter();
    let media_type = segments.next()?.trim();
    let specificity = if media_type.eq_ignore_ascii_case(JSONAPI_MEDIA_TYPE) {
        2
    } else if media_type.eq_ignore_ascii_case("application/*") {
        1
    } else if media_type == "*/*" {
        0
    } else {
        return None;
    };

    let mut quality = 1.0_f32;
    let mut has_quality = false;
    for parameter in segments {
        let (name, value) = parameter.trim().split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("q") {
            if has_quality {
                return None;
            }
            quality = value.parse().ok()?;
            if !(0.0..=1.0).contains(&quality) {
                return None;
            }
            has_quality = true;
        } else if name.eq_ignore_ascii_case("profile") {
            // Profiles are advisory; unrecognized profiles do not change this
            // endpoint's base JSON:API representation.
            if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
                return None;
            }
        } else {
            // Unsupported extensions and other parameters do not match this
            // endpoint's base-only JSON:API representation.
            return None;
        }
    }
    Some((specificity, quality))
}

fn split_quoted(value: &str, delimiter: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut segment_start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == delimiter && !quoted {
            segments.push(&value[segment_start..index]);
            segment_start = index + character.len_utf8();
        }
    }
    segments.push(&value[segment_start..]);
    segments
}

fn project_resource(
    definition: &ResourceDefinition,
    record: &AdapterResource,
) -> Result<ResourceObject, AdapterError> {
    let attributes: Map<String, Value> = definition
        .attributes()
        .iter()
        .filter_map(|mapping| {
            record
                .attributes
                .get(mapping.model_field())
                .map(|value| (mapping.public_name().to_owned(), value.clone()))
        })
        .collect();
    let relationships = definition
        .relationships()
        .iter()
        .filter_map(|mapping| {
            record
                .relationships
                .get(mapping.model_field())
                .map(|relationship| (mapping, relationship))
        })
        .map(|(mapping, relationship)| {
            if !relationship_matches_target(relationship, mapping.target_type()) {
                return Err(AdapterError);
            }
            Ok((mapping.public_name().to_owned(), relationship.clone()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    Ok(ResourceObject {
        type_name: definition.type_name().to_owned(),
        id: Some(record.id.clone()),
        lid: None,
        attributes: (!attributes.is_empty()).then_some(attributes),
        relationships: (!relationships.is_empty()).then_some(relationships),
        links: None,
        meta: None,
    })
}

fn relationship_matches_target(relationship: &Relationship, target_type: &str) -> bool {
    match relationship.data.as_ref() {
        None | Some(RelationshipData::Null) => true,
        Some(RelationshipData::One(identifier)) => identifier.type_name == target_type,
        Some(RelationshipData::Many(identifiers)) => identifiers
            .iter()
            .all(|identifier| identifier.type_name == target_type),
    }
}

fn respond_with_document(status: StatusCode, document: JsonApiDocument) -> Response {
    match document.validate_response() {
        Ok(()) => {
            let mut response = (status, Json(document)).into_response();
            set_jsonapi_headers(&mut response);
            response
        }
        Err(_) => adapter_error(),
    }
}

fn forbidden_error() -> Response {
    protocol_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "Access denied",
        Some("Access to this resource is not permitted.".to_owned()),
        None,
    )
}

fn adapter_error() -> Response {
    protocol_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "read_failed",
        "Resource read failed",
        Some("The resource could not be loaded.".to_owned()),
        None,
    )
}

fn protocol_error(
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: Option<String>,
    parameter: Option<String>,
) -> Response {
    let error = ErrorObject {
        status: Some(status.as_u16().to_string()),
        code: Some(code.to_owned()),
        title: Some(title.to_owned()),
        detail,
        source: parameter.map(|parameter| ErrorSource {
            parameter: Some(parameter),
            ..ErrorSource::default()
        }),
        ..ErrorObject::default()
    };
    let mut response = (
        status,
        Json(JsonApiDocument {
            errors: Some(vec![error]),
            ..JsonApiDocument::default()
        }),
    )
        .into_response();
    set_jsonapi_headers(&mut response);
    response
}

fn set_jsonapi_headers(response: &mut Response<Body>) {
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(JSONAPI_MEDIA_TYPE));
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept"));
}
