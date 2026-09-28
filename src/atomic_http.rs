//! Axum endpoint for JSON:API Atomic Operations.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use sea_orm::DatabaseConnection;
use serde_json::{Value, from_value};

use crate::atomic::{
    ATOMIC_OPERATIONS_EXTENSION, AtomicExecutionError, AtomicHrefResolver, AtomicOperationHandler,
    AtomicOperationsDocument, AtomicOperationsError, AtomicOperationsGuard,
    execute_atomic_operations, plan_atomic_operations, plan_atomic_operations_with_href_resolver,
};
use crate::document::is_valid_absolute_uri;
use crate::document::{ErrorObject, ErrorSource, JsonApiDocument};
use crate::json::parse_unique_members;
use crate::media::{is_http_token, unquote_http_quoted_string};
use crate::registry::ResourceRegistry;

const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";
const ATOMIC_CONTENT_TYPE: &str = "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"";

struct AtomicApiState {
    registry: Arc<ResourceRegistry>,
    database: DatabaseConnection,
    guard: Arc<dyn AtomicOperationsGuard>,
    handler: Arc<dyn AtomicOperationHandler>,
    href_resolver: Option<Arc<dyn AtomicHrefResolver>>,
}

struct MediaParameter {
    name: String,
    value: Option<String>,
    quoted: bool,
}

/// Builds a standalone `POST /operations` router for Atomic Operations.
///
/// Merge this router with the application's other routes. Requests must use
/// the Atomic Operations extension in both `Content-Type` and `Accept`.
///
/// # Errors
///
/// Request and execution errors are returned as JSON:API error documents.
pub fn router(
    registry: Arc<ResourceRegistry>,
    database: DatabaseConnection,
    guard: Arc<dyn AtomicOperationsGuard>,
    handler: Arc<dyn AtomicOperationHandler>,
) -> Router {
    build_router(registry, database, guard, handler, None)
}

/// Builds an Atomic Operations router with application route resolution for
/// relationship `href` targets.
pub fn router_with_href_resolver(
    registry: Arc<ResourceRegistry>,
    database: DatabaseConnection,
    guard: Arc<dyn AtomicOperationsGuard>,
    handler: Arc<dyn AtomicOperationHandler>,
    href_resolver: Arc<dyn AtomicHrefResolver>,
) -> Router {
    build_router(registry, database, guard, handler, Some(href_resolver))
}

fn build_router(
    registry: Arc<ResourceRegistry>,
    database: DatabaseConnection,
    guard: Arc<dyn AtomicOperationsGuard>,
    handler: Arc<dyn AtomicOperationHandler>,
    href_resolver: Option<Arc<dyn AtomicHrefResolver>>,
) -> Router {
    Router::new()
        .route(
            "/operations",
            post(post_operations).fallback(operations_method_not_allowed),
        )
        .with_state(Arc::new(AtomicApiState {
            registry,
            database,
            guard,
            handler,
            href_resolver,
        }))
}

async fn operations_method_not_allowed(headers: HeaderMap) -> Response {
    if !accepts_atomic_media_type(&headers) {
        return base_error(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            "Atomic Operations response is not acceptable",
            "Accept must include the JSON:API Atomic Operations extension.",
        );
    }
    atomic_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "Method not allowed",
        "The Atomic Operations endpoint only accepts POST requests.",
        None,
    )
}

async fn post_operations(
    State(state): State<Arc<AtomicApiState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !has_atomic_content_type(&headers) {
        return base_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Atomic Operations media type required",
            "Content-Type must include the JSON:API Atomic Operations extension.",
        );
    }
    if !accepts_atomic_media_type(&headers) {
        return base_error(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            "Atomic Operations response is not acceptable",
            "Accept must include the JSON:API Atomic Operations extension.",
        );
    }
    if query.as_deref().is_some_and(|query| !query.is_empty()) {
        return atomic_error(
            StatusCode::BAD_REQUEST,
            "unsupported_query",
            "Unsupported query parameter",
            "The Atomic Operations endpoint does not support query parameters.",
            None,
        );
    }

    let request_document: Value = match parse_unique_members(&body) {
        Ok(document) => document,
        Err(error) => {
            return atomic_error(
                StatusCode::BAD_REQUEST,
                "invalid_document",
                "Invalid Atomic Operations document",
                &error.to_string(),
                None,
            );
        }
    };
    if let Err(error) = validate_operation_shape(&request_document) {
        return atomic_request_error(error, &request_document);
    }
    let document: AtomicOperationsDocument = match from_value(request_document.clone()) {
        Ok(document) => document,
        Err(error) => {
            let pointer = request_document
                .get("atomic:operations")
                .filter(|operations| !operations.is_array())
                .map(|_| "/atomic:operations".to_owned());
            return atomic_error(
                StatusCode::BAD_REQUEST,
                "invalid_document",
                "Invalid Atomic Operations document",
                &error.to_string(),
                existing_request_pointer(pointer, &request_document),
            );
        }
    };
    if let Err(error) = document.validate_request() {
        return atomic_request_error(error, &request_document);
    }
    let planned = match &state.href_resolver {
        Some(resolver) => {
            plan_atomic_operations_with_href_resolver(&state.registry, &document, resolver.as_ref())
        }
        None => plan_atomic_operations(&state.registry, &document),
    };
    let operations = match planned {
        Ok(operations) => operations,
        Err(error) => return atomic_request_error(error, &request_document),
    };
    let results = match execute_atomic_operations(
        &state.database,
        &operations,
        &headers,
        state.guard.as_ref(),
        state.handler.as_ref(),
    )
    .await
    {
        Ok(results) => results,
        Err(error) => return atomic_execution_error(error, &request_document),
    };

    let response = AtomicOperationsDocument {
        results: Some(results),
        ..AtomicOperationsDocument::default()
    };
    if let Err(error) = response.validate_response_for(&operations) {
        return atomic_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_atomic_response",
            "Atomic Operations response failed validation",
            &error.to_string(),
            None,
        );
    }
    let mut response = (StatusCode::OK, Json(response)).into_response();
    set_jsonapi_headers(&mut response, true);
    response
}

fn validate_operation_shape(request_document: &Value) -> Result<(), AtomicOperationsError> {
    let Some(operations) = request_document
        .get("atomic:operations")
        .and_then(Value::as_array)
    else {
        return Ok(());
    };

    for (index, operation) in operations.iter().enumerate() {
        let Some(operation) = operation.as_object() else {
            return Err(AtomicOperationsError::InvalidOperation {
                index,
                pointer: format!("/atomic:operations/{index}"),
                message: "each operation must be an object".to_owned(),
            });
        };
        if !operation.get("op").is_some_and(Value::is_string) {
            return Err(AtomicOperationsError::InvalidOperation {
                index,
                pointer: format!("/atomic:operations/{index}"),
                message: "operation must contain a string `op` member".to_owned(),
            });
        }
        if operation.get("href").is_some_and(|href| !href.is_string()) {
            return Err(AtomicOperationsError::InvalidOperation {
                index,
                pointer: format!("/atomic:operations/{index}/href"),
                message: "`href` must be a string".to_owned(),
            });
        }
        if let Some(reference_value) = operation.get("ref") {
            let reference_pointer = format!("/atomic:operations/{index}/ref");
            let Some(reference) = reference_value.as_object() else {
                return Err(AtomicOperationsError::InvalidOperation {
                    index,
                    pointer: reference_pointer,
                    message: "`ref` must be an object".to_owned(),
                });
            };
            if !reference.get("type").is_some_and(Value::is_string) {
                return Err(AtomicOperationsError::InvalidOperation {
                    index,
                    pointer: reference_pointer,
                    message: "`ref` must contain a string `type` member".to_owned(),
                });
            }
            for member in ["id", "lid", "relationship"] {
                if reference
                    .get(member)
                    .is_some_and(|value| !value.is_string())
                {
                    return Err(AtomicOperationsError::InvalidOperation {
                        index,
                        pointer: format!("{reference_pointer}/{member}"),
                        message: format!("`ref.{member}` must be a string"),
                    });
                }
            }
        }
    }
    Ok(())
}

fn atomic_request_error(error: AtomicOperationsError, request_document: &Value) -> Response {
    let pointer = match &error {
        AtomicOperationsError::InvalidOperation { pointer, .. }
        | AtomicOperationsError::Forbidden { pointer, .. } => Some(pointer.clone()),
        AtomicOperationsError::MissingOperations => Some("/atomic:operations".to_owned()),
        _ => None,
    };
    if matches!(&error, AtomicOperationsError::Forbidden { .. }) {
        return atomic_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Access denied",
            &error.to_string(),
            existing_request_pointer(pointer, request_document),
        );
    }
    atomic_error(
        StatusCode::BAD_REQUEST,
        "invalid_atomic_operation",
        "Invalid Atomic Operations request",
        &error.to_string(),
        existing_request_pointer(pointer, request_document),
    )
}

fn atomic_execution_error(error: AtomicExecutionError, request_document: &Value) -> Response {
    let (status, code, title, detail, pointer) = match error {
        AtomicExecutionError::NotAuthorized => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Access denied",
            "The Atomic Operations request is not authorized.".to_owned(),
            None,
        ),
        AtomicExecutionError::LimitExceeded(message) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "resource_limit",
            "Atomic Operations request exceeds limits",
            message,
            None,
        ),
        AtomicExecutionError::Operation { index, message } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "operation_failed",
            "Atomic operation failed",
            message,
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::Conflict { index, message } => (
            StatusCode::CONFLICT,
            "conflict",
            "Atomic operation conflict",
            message,
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::NotFound { index, message } => (
            StatusCode::NOT_FOUND,
            "resource_not_found",
            "Resource not found",
            message,
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::LocalId { index, message } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "atomic_local_id_mapping_failed",
            "Atomic local ID mapping failed",
            message,
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::InvalidResult { index, message } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_atomic_response",
            "Atomic Operations response failed validation",
            message,
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::Rollback { index, .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "rollback_failed",
            "Atomic transaction rollback failed",
            "The Atomic Operations transaction could not be rolled back.".to_owned(),
            Some(format!("/atomic:operations/{index}")),
        ),
        AtomicExecutionError::Database(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "database_error",
            "Atomic transaction failed",
            "The Atomic Operations transaction could not be completed.".to_owned(),
            None,
        ),
    };
    atomic_error(
        status,
        code,
        title,
        &detail,
        existing_request_pointer(pointer, request_document),
    )
}

fn existing_request_pointer(pointer: Option<String>, request_document: &Value) -> Option<String> {
    pointer.filter(|pointer| request_document.pointer(pointer).is_some())
}

fn has_atomic_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((media_type, parameters)) = parse_parameters(value) else {
        return false;
    };
    if !media_type.eq_ignore_ascii_case(JSONAPI_MEDIA_TYPE) {
        return false;
    }
    let mut found_extension = false;
    let mut found_profile = false;
    for parameter in parameters {
        match parameter.name.to_ascii_lowercase().as_str() {
            "ext" if !found_extension => {
                found_extension = true;
                if !parameter.quoted
                    || !parameter
                        .value
                        .as_deref()
                        .is_some_and(has_only_atomic_extension)
                {
                    return false;
                }
            }
            "profile" if !found_profile => {
                if !parameter.quoted || !parameter.value.as_deref().is_some_and(has_valid_uri_list)
                {
                    return false;
                }
                found_profile = true;
            }
            _ => return false,
        }
    }
    found_extension
}

fn accepts_atomic_media_type(headers: &HeaderMap) -> bool {
    let values = headers.get_all(ACCEPT);
    if values.iter().next().is_none() {
        return false;
    }
    let mut best_match: Option<(u8, f32)> = None;
    for value in values {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for range in split_quoted(value, ',') {
            let Some((media_type, parameters)) = parse_parameters(range) else {
                continue;
            };
            let specificity = if media_type.eq_ignore_ascii_case(JSONAPI_MEDIA_TYPE) {
                2
            } else if media_type.eq_ignore_ascii_case("application/*") {
                1
            } else if media_type == "*/*" {
                0
            } else {
                continue;
            };
            let mut extension = false;
            let mut profile = false;
            let mut quality = 1.0_f32;
            let mut has_quality = false;
            let mut valid = true;
            for parameter in parameters {
                if has_quality {
                    // Parameters after q are Accept extensions, not media-type parameters.
                    continue;
                }
                match parameter.name.to_ascii_lowercase().as_str() {
                    "ext" if !extension => {
                        extension = parameter.quoted
                            && parameter
                                .value
                                .as_deref()
                                .is_some_and(has_only_atomic_extension);
                        if !extension {
                            valid = false;
                        }
                    }
                    "profile" if !profile => {
                        profile = parameter.quoted
                            && parameter.value.as_deref().is_some_and(has_valid_uri_list);
                        if !profile {
                            valid = false;
                        }
                    }
                    "q" if !has_quality => {
                        has_quality = true;
                        if parameter.quoted {
                            valid = false;
                        } else if let Some(parsed_quality) =
                            parameter.value.as_deref().and_then(parse_quality_value)
                        {
                            quality = parsed_quality;
                        } else {
                            valid = false;
                        }
                    }
                    _ => valid = false,
                }
            }
            if valid && extension {
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

fn has_only_atomic_extension(value: &str) -> bool {
    value == ATOMIC_OPERATIONS_EXTENSION
}

fn has_valid_uri_list(value: &str) -> bool {
    !value.is_empty()
        && value
            .split(' ')
            .all(|uri| !uri.is_empty() && is_valid_absolute_uri(uri))
}

fn parse_parameters(value: &str) -> Option<(String, Vec<MediaParameter>)> {
    let mut segments = split_quoted(value, ';').into_iter();
    let media_type = segments.next()?.trim();
    if media_type.is_empty() {
        return None;
    }
    let mut parameters = Vec::new();
    for segment in segments {
        let segment = segment.trim();
        let (name, raw_value) = match segment.split_once('=') {
            Some((name, raw_value)) => (name.trim(), Some(raw_value.trim())),
            None => (segment, None),
        };
        if !is_http_token(name) {
            return None;
        }
        let (value, quoted) = match raw_value {
            None => (None, false),
            Some(raw_value) => match unquote_http_quoted_string(raw_value) {
                Some(value) => (Some(value), true),
                None if is_http_token(raw_value) => (Some(raw_value.to_owned()), false),
                None => return None,
            },
        };
        parameters.push(MediaParameter {
            name: name.to_owned(),
            value,
            quoted,
        });
    }
    Some((media_type.to_owned(), parameters))
}

fn split_quoted(value: &str, delimiter: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
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
            segments.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if quoted || escaped {
        return Vec::new();
    }
    segments.push(&value[start..]);
    segments
}

fn atomic_error(
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: &str,
    pointer: Option<String>,
) -> Response {
    let error = ErrorObject {
        status: Some(status.as_u16().to_string()),
        code: Some(code.to_owned()),
        title: Some(title.to_owned()),
        detail: Some(detail.to_owned()),
        source: pointer.map(|pointer| ErrorSource {
            pointer: Some(pointer),
            ..ErrorSource::default()
        }),
        ..ErrorObject::default()
    };
    let mut response = (
        status,
        Json(AtomicOperationsDocument {
            errors: Some(vec![error]),
            ..AtomicOperationsDocument::default()
        }),
    )
        .into_response();
    set_jsonapi_headers(&mut response, true);
    response
}

fn base_error(
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: &str,
) -> Response {
    let error = ErrorObject {
        status: Some(status.as_u16().to_string()),
        code: Some(code.to_owned()),
        title: Some(title.to_owned()),
        detail: Some(detail.to_owned()),
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
    set_jsonapi_headers(&mut response, false);
    response
}

fn set_jsonapi_headers(response: &mut Response<Body>, atomic_extension: bool) {
    let content_type = if atomic_extension {
        ATOMIC_CONTENT_TYPE
    } else {
        JSONAPI_MEDIA_TYPE
    };
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_quoted_atomic_extension_parameters() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(ATOMIC_CONTENT_TYPE));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\"",
            ),
        );
        assert!(has_atomic_content_type(&headers));
        assert!(accepts_atomic_media_type(&headers));

        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.api+json;ext=https://jsonapi.org/ext/atomic"),
        );
        assert!(!has_atomic_content_type(&headers));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(ATOMIC_CONTENT_TYPE));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.api+json;ext=https://jsonapi.org/ext/atomic"),
        );
        assert!(!accepts_atomic_media_type(&headers));
    }

    #[test]
    fn accepts_only_positive_quality_atomic_jsonapi_ranges() {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0.8",
            ),
        );
        assert!(accepts_atomic_media_type(&headers));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=0",
            ),
        );
        assert!(!accepts_atomic_media_type(&headers));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";q=\"1\"",
            ),
        );
        assert!(!accepts_atomic_media_type(&headers));
    }

    #[test]
    fn validates_profile_uri_lists_in_content_type_and_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/profile https://example.test/other\"",
            ),
        );
        headers.insert(ACCEPT, HeaderValue::from_static(ATOMIC_CONTENT_TYPE));
        assert!(has_atomic_content_type(&headers));
        assert!(accepts_atomic_media_type(&headers));

        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"relative/profile\"",
            ),
        );
        assert!(!has_atomic_content_type(&headers));

        headers.insert(CONTENT_TYPE, HeaderValue::from_static(ATOMIC_CONTENT_TYPE));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "application/vnd.api+json;ext=\"https://jsonapi.org/ext/atomic\";profile=\"https://example.test/%2\"",
            ),
        );
        assert!(!accepts_atomic_media_type(&headers));
    }

    #[tokio::test]
    async fn disabled_atomic_permissions_map_to_forbidden_errors() {
        let request_document = serde_json::json!({
            "atomic:operations": [{"op": "add", "data": {"type": "authors"}}]
        });
        let response = atomic_request_error(
            AtomicOperationsError::Forbidden {
                index: 0,
                pointer: "/atomic:operations/0/data/type".to_owned(),
                message: "Atomic create is disabled".to_owned(),
            },
            &request_document,
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let document: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            document["errors"][0]["source"]["pointer"],
            "/atomic:operations/0/data/type"
        );
    }

    #[tokio::test]
    async fn rollback_failures_map_to_server_errors_with_operation_pointer() {
        let request_document = serde_json::json!({
            "atomic:operations": [
                {"op": "remove", "ref": {"type": "authors", "id": "1"}},
                {"op": "remove", "ref": {"type": "authors", "id": "2"}}
            ]
        });
        let response = atomic_execution_error(
            AtomicExecutionError::Rollback {
                index: 1,
                operation: "operation failed".to_owned(),
                rollback: sea_orm::DbErr::Custom("rollback failed".to_owned()),
            },
            &request_document,
        );

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()[CONTENT_TYPE], ATOMIC_CONTENT_TYPE);
        assert_eq!(response.headers()[VARY], "Accept");
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&bytes).unwrap();
        let error = &document["errors"][0];
        assert_eq!(error["code"], "rollback_failed");
        assert_eq!(error["status"], "500");
        assert_eq!(error["source"]["pointer"], "/atomic:operations/1");
        assert!(request_document.pointer("/atomic:operations/1").is_some());
    }
}
