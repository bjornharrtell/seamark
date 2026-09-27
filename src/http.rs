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
    ResourceObject, is_valid_absolute_uri,
};
use crate::query::{
    IncludeNode, PaginationConfig, PlannedField, ReadPlan, ReadPlanError, ReadQuery, plan_read,
    plan_resource_read,
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

/// A query adapter failure with an HTTP-relevant category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryAdapterError {
    /// The query could not be completed because of an internal failure.
    ReadFailed,
    /// The caller was denied access to the planned query.
    NotAuthorized,
    /// The planned query exceeded an application-configured execution limit.
    LimitExceeded,
    /// The adapter does not implement single-resource query execution.
    ResourceReadUnsupported,
}

/// A resource from the `included` member of a planned collection read.
#[derive(Clone, Debug, PartialEq)]
pub struct AdapterIncludedResource {
    /// The public JSON:API type of the included resource.
    pub resource_type: String,
    /// The mapped persistence record.
    pub resource: AdapterResource,
}

/// Results returned by an adapter for a planned collection read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryCollectionResult {
    /// Root collection resources.
    pub resources: Vec<AdapterResource>,
    /// Compound resources requested by the validated include plan.
    pub included: Vec<AdapterIncludedResource>,
}

/// Results returned by an adapter for a planned single-resource read.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResourceResult {
    /// The root resource.
    pub resource: AdapterResource,
    /// Compound resources requested by the validated include plan.
    pub included: Vec<AdapterIncludedResource>,
}

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

/// Executes validated collection and single-resource plans produced by
/// [`router_with_query`].
///
/// Authorization and execution-limit failures should use their corresponding
/// [`QueryAdapterError`] variants so the router can return client-appropriate
/// JSON:API error responses. Other failures are treated as internal read
/// errors.
#[async_trait]
pub trait QueryResourceAdapter: Send + Sync + 'static {
    /// Executes the plan for one registered resource.
    async fn collection(
        &self,
        resource: &ResourceDefinition,
        plan: &ReadPlan,
    ) -> Result<QueryCollectionResult, QueryAdapterError>;

    /// Executes the plan for one resource addressed by its persistent ID.
    ///
    /// The default reports the unsupported operation explicitly so existing
    /// adapters remain source-compatible while opting into single-resource
    /// query execution requires an explicit implementation.
    async fn resource(
        &self,
        _resource: &ResourceDefinition,
        _id: &str,
        _plan: &ReadPlan,
    ) -> Result<Option<QueryResourceResult>, QueryAdapterError> {
        Err(QueryAdapterError::ResourceReadUnsupported)
    }
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
    query_adapter: Option<Arc<dyn QueryResourceAdapter>>,
    pagination: Option<PaginationConfig>,
}

/// Builds the collection and single-resource GET routes without query support.
///
/// The routes reject all non-empty query strings. Filtering, sorting,
/// pagination, includes, and persistence implementations are outside this
/// adapter boundary.
pub fn router(
    registry: Arc<ResourceRegistry>,
    adapter: Arc<dyn ResourceAdapter>,
    authorizer: Arc<dyn RequestAuthorizer>,
) -> Router {
    build_router(ApiState {
        registry,
        adapter,
        authorizer,
        query_adapter: None,
        pagination: None,
    })
}

/// Builds collection and single-resource GET routes with planned queries.
///
/// Query support is opt-in. The supplied pagination policy is explicit, and
/// the query adapter is called only after parsing, planning, resource lookup,
/// and request authorization. Collection filters, sorting, and pagination are
/// supported only on collections; single-resource reads support includes and
/// sparse fieldsets.
pub fn router_with_query(
    registry: Arc<ResourceRegistry>,
    adapter: Arc<dyn ResourceAdapter>,
    authorizer: Arc<dyn RequestAuthorizer>,
    query_adapter: Arc<dyn QueryResourceAdapter>,
    pagination: PaginationConfig,
) -> Router {
    build_router(ApiState {
        registry,
        adapter,
        authorizer,
        query_adapter: Some(query_adapter),
        pagination: Some(pagination),
    })
}

fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/{resource_type}", get(get_collection))
        .route("/{resource_type}/{id}", get(get_resource))
        .with_state(Arc::new(state))
}

async fn get_collection(
    State(state): State<Arc<ApiState>>,
    Path(resource_type): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !accepts_jsonapi(&headers) {
        return request_error_response(RequestValidationError::NotAcceptable);
    }
    let has_query = query.as_deref().is_some_and(|query| !query.is_empty());
    if has_query && state.query_adapter.is_none() {
        return request_error_response(RequestValidationError::UnsupportedQuery(
            query.as_deref().and_then(first_query_parameter),
        ));
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
    let plan = if has_query {
        let Some(pagination) = state.pagination.as_ref() else {
            return request_error_response(RequestValidationError::UnsupportedQuery(
                query.as_deref().and_then(first_query_parameter),
            ));
        };
        let query = match parse_read_query(query.as_deref().unwrap_or_default()) {
            Ok(query) => query,
            Err(error) => return query_parse_error(error),
        };
        match plan_read(&state.registry, &resource_type, &query, pagination) {
            Ok(plan) => Some(plan),
            Err(error) => return read_plan_error(error),
        }
    } else {
        None
    };
    if !state
        .authorizer
        .authorize(&resource_type, None, &headers)
        .await
    {
        return forbidden_error();
    }

    let (records, included) =
        if let (Some(query_adapter), Some(plan)) = (state.query_adapter.as_ref(), plan.as_ref()) {
            match query_adapter.collection(definition, plan).await {
                Ok(result) => (result.resources, result.included),
                Err(error) => return query_adapter_error(error),
            }
        } else {
            match state.adapter.collection(definition).await {
                Ok(records) => (records, Vec::new()),
                Err(_) => return adapter_error(),
            }
        };
    let resources = match records
        .iter()
        .map(|record| project_resource(definition, record))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(resources) => resources,
        Err(_) => return adapter_error(),
    };
    let included = match included
        .iter()
        .map(|included| {
            let definition = state
                .registry
                .resource(&included.resource_type)
                .map_err(|_| AdapterError)?;
            project_resource(definition, &included.resource)
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(included) => included,
        Err(_) => return adapter_error(),
    };
    let mut document = JsonApiDocument {
        data: Some(PrimaryData::Many(resources)),
        included: (!included.is_empty()).then_some(included),
        ..JsonApiDocument::default()
    };
    if document.validate_response().is_err() {
        let sparse_fieldset_exception_applies = document.included.is_some()
            && plan
                .as_ref()
                .is_some_and(has_sparse_fieldset_include_relationship)
            && document
                .validate_response_with_sparse_fieldset_exception()
                .is_ok();
        if !sparse_fieldset_exception_applies {
            return adapter_error();
        }
    }
    if let Some(plan) = plan.as_ref() {
        if let (Some(PrimaryData::Many(resources)), Some(fieldset)) = (
            document.data.as_mut(),
            plan.fieldsets.get(definition.type_name()),
        ) {
            for resource in resources {
                apply_fieldset(resource, fieldset);
            }
        }
        if let Some(included) = document.included.as_mut() {
            for resource in included {
                if let Some(fieldset) = plan.fieldsets.get(&resource.type_name) {
                    apply_fieldset(resource, fieldset);
                }
            }
        }
    }
    respond_with_validated_document(StatusCode::OK, document)
}

async fn get_resource(
    State(state): State<Arc<ApiState>>,
    Path((resource_type, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = validate_request(&headers, None) {
        return request_error_response(error);
    }
    let has_query = query.as_deref().is_some_and(|query| !query.is_empty());
    if has_query && state.query_adapter.is_none() {
        return request_error_response(RequestValidationError::UnsupportedQuery(
            query.as_deref().and_then(first_query_parameter),
        ));
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
    let plan = if has_query {
        let Some(pagination) = state.pagination.as_ref() else {
            return request_error_response(RequestValidationError::UnsupportedQuery(
                query.as_deref().and_then(first_query_parameter),
            ));
        };
        let query = match parse_read_query(query.as_deref().unwrap_or_default()) {
            Ok(query) => query,
            Err(error) => return query_parse_error(error),
        };
        match plan_resource_read(&state.registry, &resource_type, &query, pagination) {
            Ok(plan) => Some(plan),
            Err(error) => return read_plan_error(error),
        }
    } else {
        None
    };
    if !state
        .authorizer
        .authorize(&resource_type, Some(&id), &headers)
        .await
    {
        return forbidden_error();
    }

    let (record, included) =
        if let (Some(query_adapter), Some(plan)) = (state.query_adapter.as_ref(), plan.as_ref()) {
            match query_adapter.resource(definition, &id, plan).await {
                Ok(Some(result)) => (result.resource, result.included),
                Ok(None) => {
                    return protocol_error(
                        StatusCode::NOT_FOUND,
                        "resource_not_found",
                        "Resource not found",
                        Some(format!("No `{resource_type}` resource has id `{id}`.")),
                        None,
                    );
                }
                Err(error) => return query_adapter_error(error),
            }
        } else {
            match state.adapter.resource(definition, &id).await {
                Ok(Some(record)) => (record, Vec::new()),
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
            }
        };
    let resource = match project_resource(definition, &record) {
        Ok(resource) => resource,
        Err(_) => return adapter_error(),
    };
    let included = match included
        .iter()
        .map(|included| {
            let definition = state
                .registry
                .resource(&included.resource_type)
                .map_err(|_| AdapterError)?;
            project_resource(definition, &included.resource)
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(included) => included,
        Err(_) => return adapter_error(),
    };
    let mut document = JsonApiDocument {
        data: Some(PrimaryData::One(resource)),
        included: (!included.is_empty()).then_some(included),
        ..JsonApiDocument::default()
    };
    if document.validate_response().is_err() {
        let sparse_fieldset_exception_applies = document.included.is_some()
            && plan
                .as_ref()
                .is_some_and(has_sparse_fieldset_include_relationship)
            && document
                .validate_response_with_sparse_fieldset_exception()
                .is_ok();
        if !sparse_fieldset_exception_applies {
            return adapter_error();
        }
    }
    if let Some(plan) = plan.as_ref() {
        if let (Some(PrimaryData::One(resource)), Some(fieldset)) = (
            document.data.as_mut(),
            plan.fieldsets.get(definition.type_name()),
        ) {
            apply_fieldset(resource, fieldset);
        }
        if let Some(included) = document.included.as_mut() {
            for resource in included {
                if let Some(fieldset) = plan.fieldsets.get(&resource.type_name) {
                    apply_fieldset(resource, fieldset);
                }
            }
        }
    }
    respond_with_validated_document(StatusCode::OK, document)
}

struct QueryParseError {
    parameter: String,
    detail: String,
}

fn parse_read_query(query: &str) -> Result<ReadQuery, QueryParseError> {
    let mut parsed = ReadQuery::default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = decode_query_component(raw_name).map_err(|detail| QueryParseError {
            parameter: "query".to_owned(),
            detail: detail.to_owned(),
        })?;
        let value = decode_query_component(raw_value).map_err(|detail| QueryParseError {
            parameter: name.clone(),
            detail: detail.to_owned(),
        })?;
        match name.as_str() {
            "filter" => parsed.filters.push(value),
            "sort" => set_unique_query_value(&mut parsed.sort, &name, value)?,
            "page[number]" => set_unique_query_value(&mut parsed.page_number, &name, value)?,
            "page[size]" => set_unique_query_value(&mut parsed.page_size, &name, value)?,
            "include" => parsed.includes.push(value),
            _ => {
                if let Some(resource_type) = name
                    .strip_prefix("fields[")
                    .and_then(|name| name.strip_suffix(']'))
                {
                    if resource_type.is_empty() {
                        return Err(QueryParseError {
                            parameter: name,
                            detail: "fieldset resource type must not be empty".to_owned(),
                        });
                    }
                    if parsed
                        .fieldsets
                        .insert(resource_type.to_owned(), value)
                        .is_some()
                    {
                        return Err(QueryParseError {
                            parameter: name,
                            detail: "fieldset parameter must not be repeated".to_owned(),
                        });
                    }
                } else {
                    parsed.unsupported_parameters.push(name);
                }
            }
        }
    }
    Ok(parsed)
}

fn set_unique_query_value(
    destination: &mut Option<String>,
    parameter: &str,
    value: String,
) -> Result<(), QueryParseError> {
    if destination.replace(value).is_some() {
        return Err(QueryParseError {
            parameter: parameter.to_owned(),
            detail: "query parameter must not be repeated".to_owned(),
        });
    }
    Ok(())
}

fn decode_query_component(input: &str) -> Result<String, &'static str> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err("invalid percent-encoding");
                }
                let high = hex_digit(bytes[index + 1]).ok_or("invalid percent-encoding")?;
                let low = hex_digit(bytes[index + 2]).ok_or("invalid percent-encoding")?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| "query parameter is not valid UTF-8")
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn query_parse_error(error: QueryParseError) -> Response {
    protocol_error(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "Invalid query parameter",
        Some(error.detail),
        Some(error.parameter),
    )
}

fn read_plan_error(error: ReadPlanError) -> Response {
    let parameter = match &error {
        ReadPlanError::UnsupportedQueryParameters(parameters) => parameters.first().cloned(),
        ReadPlanError::Filter(_) => Some("filter".to_owned()),
        ReadPlanError::InvalidSort(_)
        | ReadPlanError::DuplicateSortField(_)
        | ReadPlanError::UnknownSortAttribute { .. }
        | ReadPlanError::AttributeNotSortable { .. } => Some("sort".to_owned()),
        ReadPlanError::UnknownFieldsetField { resource_type, .. }
        | ReadPlanError::InvalidFieldset { resource_type, .. } => {
            Some(format!("fields[{resource_type}]"))
        }
        ReadPlanError::InvalidIncludePath(_) | ReadPlanError::UnknownRelationship { .. } => {
            Some("include".to_owned())
        }
        ReadPlanError::InvalidPageParameter { parameter, .. } => Some((*parameter).to_owned()),
        ReadPlanError::PageSizeExceedsMaximum { .. } => Some("page[size]".to_owned()),
        ReadPlanError::PageOffsetOverflow => Some("page[number]".to_owned()),
        ReadPlanError::UnknownResourceType(resource_type) => {
            Some(format!("fields[{resource_type}]"))
        }
        ReadPlanError::InvalidPaginationConfig(_)
        | ReadPlanError::PageOffsetExceedsMaximum { .. } => None,
    };
    protocol_error(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "Invalid query",
        Some(error.to_string()),
        parameter,
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
    let mut has_profile = false;
    for parameter in segments {
        if has_quality {
            // Parameters after q are Accept extensions, not media-type
            // parameters, and do not affect this representation.
            continue;
        }
        let (name, value) = parameter.trim().split_once('=')?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("q") {
            quality = parse_quality_value(value)?;
            has_quality = true;
        } else if name.eq_ignore_ascii_case("profile") {
            // Profiles are advisory; unrecognized profiles do not change this
            // endpoint's base JSON:API representation.
            let profile_uris = value.strip_prefix('"')?.strip_suffix('"')?;
            if has_profile || !has_valid_uri_list(profile_uris) {
                return None;
            }
            has_profile = true;
        } else {
            // Unsupported extensions and other parameters do not match this
            // endpoint's base-only JSON:API representation.
            return None;
        }
    }
    Some((specificity, quality))
}

fn parse_quality_value(value: &str) -> Option<f32> {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if fractional.len() > 3 || !fractional.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => value.parse().ok(),
        "1" if fractional.bytes().all(|digit| digit == b'0') => value.parse().ok(),
        _ => None,
    }
}

fn has_valid_uri_list(value: &str) -> bool {
    !value.is_empty()
        && value
            .split(' ')
            .all(|uri| !uri.is_empty() && is_valid_absolute_uri(uri))
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

fn apply_fieldset(resource: &mut ResourceObject, fieldset: &[PlannedField]) {
    let attributes = fieldset
        .iter()
        .filter_map(|field| match field {
            PlannedField::Attribute { public_name, .. } => Some(public_name.as_str()),
            PlannedField::Relationship { .. } => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let relationships = fieldset
        .iter()
        .filter_map(|field| match field {
            PlannedField::Attribute { .. } => None,
            PlannedField::Relationship { public_name, .. } => Some(public_name.as_str()),
        })
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(resource_attributes) = resource.attributes.as_mut() {
        resource_attributes.retain(|name, _| attributes.contains(name.as_str()));
        if resource_attributes.is_empty() {
            resource.attributes = None;
        }
    }
    if let Some(resource_relationships) = resource.relationships.as_mut() {
        resource_relationships.retain(|name, _| relationships.contains(name.as_str()));
        if resource_relationships.is_empty() {
            resource.relationships = None;
        }
    }
}

fn has_sparse_fieldset_include_relationship(plan: &ReadPlan) -> bool {
    fn has_hidden_relationship(
        resource_type: &str,
        includes: &[IncludeNode],
        fieldsets: &BTreeMap<String, Vec<PlannedField>>,
    ) -> bool {
        includes.iter().any(|include| {
            let relationship_is_hidden = fieldsets.get(resource_type).is_some_and(|fieldset| {
                !fieldset.iter().any(|field| {
                    matches!(
                        field,
                        PlannedField::Relationship { public_name, .. }
                            if public_name == &include.public_name
                    )
                })
            });
            relationship_is_hidden
                || has_hidden_relationship(&include.target_type, &include.children, fieldsets)
        })
    }

    has_hidden_relationship(&plan.resource_type, &plan.includes, &plan.fieldsets)
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

fn respond_with_validated_document(status: StatusCode, document: JsonApiDocument) -> Response {
    let mut response = (status, Json(document)).into_response();
    set_jsonapi_headers(&mut response);
    response
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

fn query_adapter_error(error: QueryAdapterError) -> Response {
    match error {
        QueryAdapterError::ReadFailed => adapter_error(),
        QueryAdapterError::NotAuthorized => forbidden_error(),
        QueryAdapterError::ResourceReadUnsupported => protocol_error(
            StatusCode::NOT_IMPLEMENTED,
            "resource_query_not_supported",
            "Single-resource queries are not implemented",
            Some("The configured query adapter cannot execute single-resource reads.".to_owned()),
            None,
        ),
        QueryAdapterError::LimitExceeded => protocol_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "resource_limit",
            "Query exceeds configured limits",
            Some("The requested query exceeds the server's configured limits.".to_owned()),
            None,
        ),
    }
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
