//! Planning and transaction orchestration for JSON:API Atomic Operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use async_trait::async_trait;
use axum::http::{HeaderMap, Uri};
use sea_orm::{DatabaseConnection, DatabaseTransaction, DbErr, TransactionTrait};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::document::{ErrorObject, JsonApiObject, RelationshipData, ResourceIdentifier};
use crate::registry::ResourceRegistry;

/// The extension URI required for JSON:API Atomic Operations.
pub const ATOMIC_OPERATIONS_EXTENSION: &str = "https://jsonapi.org/ext/atomic";

/// A JSON:API document carrying Atomic Operations members.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicOperationsDocument {
    /// Operations in the order the server must execute them.
    #[serde(
        rename = "atomic:operations",
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub operations: Option<Vec<AtomicOperation>>,
    /// Results in positional correspondence with a successful request.
    #[serde(
        rename = "atomic:results",
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub results: Option<Vec<AtomicResult>>,
    /// Errors for a failed request or response.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub errors: Option<Vec<ErrorObject>>,
    /// Links related to the document.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub links: Option<Map<String, Value>>,
    /// Non-standard document metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
    /// JSON:API version and capability information.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub jsonapi: Option<JsonApiObject>,
}

impl AtomicOperationsDocument {
    /// Validates that this is a request document with operations and no results or errors.
    ///
    /// # Errors
    ///
    /// Returns an error if the document has the wrong top-level members.
    pub fn validate_request(&self) -> Result<&[AtomicOperation], AtomicOperationsError> {
        if self.results.is_some() {
            return Err(AtomicOperationsError::InvalidDocument(
                "an operations request must not contain `atomic:results`",
            ));
        }
        if self.errors.is_some() {
            return Err(AtomicOperationsError::InvalidDocument(
                "an operations request must not contain `errors`",
            ));
        }
        self.operations
            .as_deref()
            .ok_or(AtomicOperationsError::MissingOperations)
    }

    /// Validates a response document and checks its positional result count.
    ///
    /// # Errors
    ///
    /// Returns an error if the response contains operations/errors, omits
    /// results, or returns a different result count than the request.
    pub fn validate_response(
        &self,
        expected_results: usize,
    ) -> Result<&[AtomicResult], AtomicOperationsError> {
        if self.operations.is_some() {
            return Err(AtomicOperationsError::InvalidDocument(
                "an operations response must not contain `atomic:operations`",
            ));
        }
        if self.errors.is_some() {
            return Err(AtomicOperationsError::InvalidDocument(
                "an operations response must not contain `errors`",
            ));
        }
        let results = self
            .results
            .as_deref()
            .ok_or(AtomicOperationsError::MissingResults)?;
        if results.len() != expected_results {
            return Err(AtomicOperationsError::ResultCountMismatch {
                expected: expected_results,
                actual: results.len(),
            });
        }
        Ok(results)
    }
}

/// One raw Atomic Operations operation object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicOperation {
    /// The operation code: `add`, `update`, or `remove`.
    pub op: String,
    /// The resource or relationship reference.
    #[serde(
        rename = "ref",
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub reference: Option<AtomicResourceReference>,
    /// An optional URI-reference target.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub href: Option<String>,
    /// Operation primary data; presence is distinct from explicit JSON `null`.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    /// Non-standard operation metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

/// A resource or relationship reference in an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicResourceReference {
    /// The public resource type of the target resource.
    #[serde(rename = "type")]
    pub type_name: String,
    /// The persistent target resource identifier.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// A local identifier established by an earlier add operation.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub lid: Option<String>,
    /// The public relationship name when the operation targets a relationship.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub relationship: Option<String>,
}

/// A resource object supplied as operation data.
///
/// Unlike a response resource object, an add request may omit both `id` and
/// `lid` when the server assigns its identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicResourceData {
    /// The public resource type.
    #[serde(rename = "type")]
    pub type_name: String,
    /// A client-provided persistent identifier.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// A request-local identifier for cross-operation references.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub lid: Option<String>,
    /// Supplied attributes; omission is distinct from explicit null values.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub attributes: Option<Map<String, Value>>,
    /// Supplied relationships.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub relationships: Option<BTreeMap<String, crate::document::Relationship>>,
    /// Resource links.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub links: Option<Map<String, Value>>,
    /// Resource metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

/// One result object in a successful Atomic Operations response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtomicResult {
    /// Primary data produced by the operation, when required or returned.
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    /// Non-standard result metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

/// One normalized operation accepted by the registry-aware planner.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedAtomicOperation {
    /// The normalized operation.
    pub operation: PlannedOperation,
    /// Operation metadata retained for application hooks.
    pub meta: Option<Map<String, Value>>,
}

/// A supported operation after request validation and public-field resolution.
#[derive(Clone, Debug, PartialEq)]
pub enum PlannedOperation {
    /// Create a resource. The optional `href` can target a distinct collection.
    AddResource {
        /// Optional URI-reference collection target.
        href: Option<String>,
        /// Validated resource data.
        data: AtomicResourceData,
        /// Request-scoped changeset with public fields resolved to model fields.
        changeset: AtomicResourceChangeset,
    },
    /// Add members to a to-many relationship.
    AddRelationshipMembers {
        /// The target relationship.
        reference: AtomicResourceReference,
        /// The mapped internal relationship field.
        model_field: String,
        /// New relationship linkage members.
        data: Vec<ResourceIdentifier>,
    },
    /// Update a resource object.
    UpdateResource {
        /// The explicit or data-derived target.
        target: AtomicTarget,
        /// Validated update data.
        data: AtomicResourceData,
        /// Request-scoped changeset with public fields resolved to model fields.
        changeset: AtomicResourceChangeset,
    },
    /// Replace a relationship's linkage.
    UpdateRelationship {
        /// The target relationship.
        reference: AtomicResourceReference,
        /// The mapped internal relationship field.
        model_field: String,
        /// Replacement linkage, including explicit `null`.
        data: RelationshipData,
    },
    /// Remove a resource.
    RemoveResource {
        /// The resource target.
        target: AtomicTarget,
    },
    /// Remove members from a to-many relationship.
    RemoveRelationshipMembers {
        /// The target relationship.
        reference: AtomicResourceReference,
        /// The mapped internal relationship field.
        model_field: String,
        /// Linkage members to remove.
        data: Vec<ResourceIdentifier>,
    },
}

/// An explicitly mapped resource mutation changeset.
///
/// `None` means that the corresponding property was omitted. An attribute
/// explicitly set to JSON `null` remains present with a null value. Relationship
/// keys use internal model fields, and their `data` preserves omitted linkage
/// separately from explicit null or empty linkage.
#[derive(Clone, Debug, PartialEq)]
pub struct AtomicResourceChangeset {
    /// The public JSON:API type.
    pub type_name: String,
    /// The configured internal identifier field.
    pub identifier_field: String,
    /// The persistent or request-local resource identity, when supplied.
    pub id: Option<String>,
    /// The request-local resource identity, when supplied.
    pub lid: Option<String>,
    /// Attributes keyed by their mapped model fields.
    pub attributes: Option<BTreeMap<String, Value>>,
    /// Relationships keyed by their mapped model fields.
    pub relationships: Option<BTreeMap<String, MappedRelationshipChange>>,
}

/// A mapped relationship included in a resource changeset.
#[derive(Clone, Debug, PartialEq)]
pub struct MappedRelationshipChange {
    /// Linkage to persist, or `None` when the relationship object omitted `data`.
    pub data: Option<RelationshipData>,
}

/// The target of a resource-level operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicTarget {
    /// Target identified by type and id/lid.
    Reference(AtomicResourceReference),
    /// Target identified by an application route.
    Href(String),
}

/// Resolves application routes used as Atomic Operations `href` targets.
///
/// Return `Ok(None)` when the URI-reference is not a route of that target
/// kind. Resolved references are validated against the resource registry and
/// local IDs before any transaction begins.
pub trait AtomicHrefResolver: Send + Sync {
    /// Maps a relationship URI-reference to its resource and public relationship.
    ///
    /// # Errors
    ///
    /// Returns a description when the URI-reference is a route but cannot be
    /// resolved to a relationship target.
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String>;

    /// Maps a resource URI-reference to its registered persistent identity.
    ///
    /// The default leaves resource routes unresolved for application-specific
    /// operation handlers.
    ///
    /// # Errors
    ///
    /// Returns a description when the URI-reference is a resource route but
    /// cannot be resolved.
    fn resolve_resource(&self, _href: &str) -> Result<Option<AtomicResourceReference>, String> {
        Ok(None)
    }

    /// Maps a collection URI-reference to its registered public resource type.
    ///
    /// The default leaves collection routes unresolved for application-specific
    /// operation handlers.
    ///
    /// # Errors
    ///
    /// Returns a description when the URI-reference is a collection route but
    /// cannot be resolved.
    fn resolve_collection(&self, _href: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// An error during Atomic Operations request validation or planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtomicOperationsError {
    /// A request document is missing its operations array.
    MissingOperations,
    /// A response document is missing its results array.
    MissingResults,
    /// Top-level Atomic Operations members are inconsistent.
    InvalidDocument(&'static str),
    /// The results array does not match the request length.
    ResultCountMismatch {
        /// The number of requested operations.
        expected: usize,
        /// The number of returned results.
        actual: usize,
    },
    /// One operation is malformed or incompatible with its operation code.
    InvalidOperation {
        /// Zero-based operation index.
        index: usize,
        /// A JSON Pointer into the operations document.
        pointer: String,
        /// A concise validation explanation.
        message: String,
    },
}

impl fmt::Display for AtomicOperationsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOperations => {
                formatter.write_str("an Atomic Operations request must contain `atomic:operations`")
            }
            Self::MissingResults => {
                formatter.write_str("an Atomic Operations response must contain `atomic:results`")
            }
            Self::InvalidDocument(message) => formatter.write_str(message),
            Self::ResultCountMismatch { expected, actual } => write!(
                formatter,
                "Atomic Operations response has {actual} results for {expected} operations"
            ),
            Self::InvalidOperation {
                index,
                pointer,
                message,
            } => write!(
                formatter,
                "invalid operation {index} at `{pointer}`: {message}"
            ),
        }
    }
}

impl std::error::Error for AtomicOperationsError {}

/// Validates and plans all operations in order.
///
/// Resource types, attributes, relationships, relationship target types, and
/// local-ID references are checked before a caller begins a database
/// transaction. A local ID may be referenced only after its add operation.
///
/// # Errors
///
/// Returns an error for malformed operation shapes, unknown registry fields,
/// mismatched relationship targets, or unresolved/duplicate local IDs.
pub fn plan_atomic_operations(
    registry: &ResourceRegistry,
    document: &AtomicOperationsDocument,
) -> Result<Vec<PlannedAtomicOperation>, AtomicOperationsError> {
    plan_atomic_operations_inner(registry, document, None)
}

/// Validates operations and resolves relationship `href` targets through the
/// application's route table.
///
/// Resolved resource and relationship routes are normalized to registered
/// references. Collection routes are checked against the added resource type.
/// Unresolved routes remain URI references for application-specific handlers.
///
/// # Errors
///
/// Returns an error for malformed documents, unresolved relationship routes,
/// invalid operations, or unknown registry fields.
pub fn plan_atomic_operations_with_href_resolver(
    registry: &ResourceRegistry,
    document: &AtomicOperationsDocument,
    resolver: &dyn AtomicHrefResolver,
) -> Result<Vec<PlannedAtomicOperation>, AtomicOperationsError> {
    plan_atomic_operations_inner(registry, document, Some(resolver))
}

fn plan_atomic_operations_inner(
    registry: &ResourceRegistry,
    document: &AtomicOperationsDocument,
    href_resolver: Option<&dyn AtomicHrefResolver>,
) -> Result<Vec<PlannedAtomicOperation>, AtomicOperationsError> {
    let operations = document.validate_request()?;
    let mut local_ids = BTreeSet::<(String, String)>::new();
    let mut planned = Vec::with_capacity(operations.len());

    for (index, operation) in operations.iter().enumerate() {
        let path = format!("/atomic:operations/{index}");
        let fail = |message: String| AtomicOperationsError::InvalidOperation {
            index,
            pointer: path.clone(),
            message,
        };
        validate_operation_target(registry, operation, index, &path, &local_ids)?;
        let href_relationship = href_resolver
            .zip(operation.href.as_deref())
            .map(|(resolver, href)| resolver.resolve_relationship(href))
            .transpose()
            .map_err(|message| fail(format!("could not resolve `href`: {message}")))?
            .flatten();
        if let Some(reference) = &href_relationship {
            validate_reference(registry, reference, &local_ids)
                .map_err(|message| fail(format!("invalid relationship `href`: {message}")))?;
            if reference.relationship.is_none() {
                return Err(fail(
                    "relationship `href` did not resolve to a relationship".to_owned(),
                ));
            }
        }
        let href_resource = if operation.op != "add" && href_relationship.is_none() {
            href_resolver
                .zip(operation.href.as_deref())
                .map(|(resolver, href)| resolver.resolve_resource(href))
                .transpose()
                .map_err(|message| fail(format!("could not resolve resource `href`: {message}")))?
                .flatten()
        } else {
            None
        };
        if let Some(reference) = &href_resource {
            validate_reference(registry, reference, &local_ids)
                .map_err(|message| fail(format!("invalid resource `href`: {message}")))?;
            if reference.relationship.is_some() {
                return Err(fail(
                    "resource `href` resolved to a relationship target".to_owned(),
                ));
            }
        }
        let href_collection = if operation.op == "add" && href_relationship.is_none() {
            href_resolver
                .zip(operation.href.as_deref())
                .map(|(resolver, href)| resolver.resolve_collection(href))
                .transpose()
                .map_err(|message| fail(format!("could not resolve collection `href`: {message}")))?
                .flatten()
        } else {
            None
        };
        let kind = operation.op.as_str();

        let planned_operation = match kind {
            "add" => {
                let relationship_reference = operation
                    .reference
                    .as_ref()
                    .filter(|reference| reference.relationship.is_some())
                    .or(href_relationship.as_ref());
                if operation.reference.is_some() && relationship_reference.is_none() {
                    return Err(fail(
                        "an add operation with `ref` must target a relationship".to_owned(),
                    ));
                }
                if let Some(reference) = relationship_reference {
                    let relationship = reference.relationship.as_deref().unwrap_or_default();
                    let data = parse_relationship_data(operation, index, &path)?;
                    let RelationshipData::Many(identifiers) = data else {
                        return Err(fail(
                            "adding relationship members requires an array of identifiers"
                                .to_owned(),
                        ));
                    };
                    let target = registry
                        .relationship(&reference.type_name, relationship)
                        .expect("relationship reference was checked");
                    let model_field = target.model_field().to_owned();
                    validate_linkage(
                        registry,
                        target.target_type(),
                        &identifiers,
                        &local_ids,
                        index,
                        &path,
                    )?;
                    PlannedOperation::AddRelationshipMembers {
                        reference: reference.clone(),
                        model_field,
                        data: identifiers,
                    }
                } else {
                    let data = parse_resource_data(operation, index, &path)?;
                    if href_collection
                        .as_deref()
                        .is_some_and(|type_name| type_name != data.type_name)
                    {
                        return Err(fail(
                            "collection `href` type must match the resource data type".to_owned(),
                        ));
                    }
                    if data.id.is_some() && data.lid.is_some() {
                        return Err(fail(
                            "resource data must not contain both `id` and `lid`".to_owned(),
                        ));
                    }
                    let changeset =
                        validate_resource_data(registry, &data, &local_ids, index, &path, true)?;
                    if let Some(lid) = &data.lid {
                        insert_local_id(&mut local_ids, &data.type_name, lid, index, &path)?;
                    }
                    PlannedOperation::AddResource {
                        href: if href_collection.is_some() {
                            None
                        } else {
                            operation.href.clone()
                        },
                        data,
                        changeset,
                    }
                }
            }
            "update" => {
                let relationship_reference = operation
                    .reference
                    .as_ref()
                    .filter(|reference| reference.relationship.is_some())
                    .or(href_relationship.as_ref());
                if let Some(reference) = relationship_reference {
                    let relationship = reference.relationship.as_deref().unwrap_or_default();
                    let data = parse_relationship_data(operation, index, &path)?;
                    let target = registry
                        .relationship(&reference.type_name, relationship)
                        .expect("relationship reference was checked");
                    let model_field = target.model_field().to_owned();
                    validate_relationship_data(
                        registry,
                        target.target_type(),
                        &data,
                        &local_ids,
                        index,
                        &path,
                    )?;
                    PlannedOperation::UpdateRelationship {
                        reference: reference.clone(),
                        model_field,
                        data,
                    }
                } else {
                    let data = parse_resource_data(operation, index, &path)?;
                    let changeset =
                        validate_resource_data(registry, &data, &local_ids, index, &path, false)?;
                    let target = resource_target(operation, &data, index, &path)?;
                    let target = if let (AtomicTarget::Href(_), Some(reference)) =
                        (&target, href_resource.as_ref())
                    {
                        AtomicTarget::Reference(reference.clone())
                    } else {
                        target
                    };
                    if let AtomicTarget::Reference(reference) = &target {
                        if reference.type_name != data.type_name {
                            return Err(fail(
                                "the operation target type must match the resource data type"
                                    .to_owned(),
                            ));
                        }
                        if (data.id.is_some() || data.lid.is_some())
                            && (data.id != reference.id || data.lid != reference.lid)
                        {
                            return Err(fail(
                                "the operation target identity must match the resource data"
                                    .to_owned(),
                            ));
                        }
                    }
                    PlannedOperation::UpdateResource {
                        target,
                        data,
                        changeset,
                    }
                }
            }
            "remove" => {
                let relationship_reference = operation
                    .reference
                    .as_ref()
                    .filter(|reference| reference.relationship.is_some())
                    .or(href_relationship.as_ref());
                if let Some(reference) = relationship_reference {
                    let relationship = reference.relationship.as_deref().unwrap_or_default();
                    let data = parse_relationship_data(operation, index, &path)?;
                    let RelationshipData::Many(identifiers) = data else {
                        return Err(fail(
                            "removing relationship members requires an array of identifiers"
                                .to_owned(),
                        ));
                    };
                    let target = registry
                        .relationship(&reference.type_name, relationship)
                        .expect("relationship reference was checked");
                    let model_field = target.model_field().to_owned();
                    validate_linkage(
                        registry,
                        target.target_type(),
                        &identifiers,
                        &local_ids,
                        index,
                        &path,
                    )?;
                    PlannedOperation::RemoveRelationshipMembers {
                        reference: reference.clone(),
                        model_field,
                        data: identifiers,
                    }
                } else {
                    if operation.data.is_some() {
                        return Err(fail(
                            "removing a resource must not include `data`".to_owned(),
                        ));
                    }
                    let target = if let Some(reference) = href_resource {
                        AtomicTarget::Reference(reference)
                    } else {
                        operation_target(operation)
                            .ok_or_else(|| fail("remove requires `ref` or `href`".to_owned()))?
                    };
                    PlannedOperation::RemoveResource { target }
                }
            }
            _ => {
                return Err(fail(format!(
                    "unsupported operation code `{}`",
                    operation.op
                )));
            }
        };
        planned.push(PlannedAtomicOperation {
            operation: planned_operation,
            meta: operation.meta.clone(),
        });
    }

    Ok(planned)
}

fn validate_operation_target(
    registry: &ResourceRegistry,
    operation: &AtomicOperation,
    index: usize,
    path: &str,
    local_ids: &BTreeSet<(String, String)>,
) -> Result<(), AtomicOperationsError> {
    let fail = |message: String| AtomicOperationsError::InvalidOperation {
        index,
        pointer: path.to_owned(),
        message,
    };
    if operation.reference.is_some() && operation.href.is_some() {
        return Err(fail(
            "an operation must not contain both `ref` and `href`".to_owned(),
        ));
    }
    if operation.href.as_deref().is_some_and(str::is_empty) {
        return Err(fail("`href` must not be empty".to_owned()));
    }
    if operation
        .href
        .as_deref()
        .is_some_and(|href| href.parse::<Uri>().is_err())
    {
        return Err(fail("`href` must be a valid URI-reference".to_owned()));
    }
    if let Some(reference) = &operation.reference {
        validate_reference(registry, reference, local_ids).map_err(fail)?;
    }
    if operation.op == "update" && operation.data.is_none() {
        return Err(fail("an update operation requires `data`".to_owned()));
    }
    if operation.op == "add" && operation.data.is_none() {
        return Err(fail("an add operation requires `data`".to_owned()));
    }
    Ok(())
}

fn validate_reference(
    registry: &ResourceRegistry,
    reference: &AtomicResourceReference,
    local_ids: &BTreeSet<(String, String)>,
) -> Result<(), String> {
    if reference.type_name.is_empty() {
        return Err("reference `type` must not be empty".to_owned());
    }
    let definition = registry
        .resource(&reference.type_name)
        .map_err(|_| format!("resource type `{}` is not registered", reference.type_name))?;
    match (&reference.id, &reference.lid) {
        (Some(id), None) if !id.is_empty() => {}
        (None, Some(lid)) if !lid.is_empty() => {
            if !local_ids.contains(&(reference.type_name.clone(), lid.clone())) {
                return Err(format!(
                    "local id `{lid}` for resource type `{}` has not been added earlier",
                    reference.type_name
                ));
            }
        }
        (Some(_), Some(_)) => {
            return Err("a reference must contain `id` or `lid`, not both".to_owned());
        }
        _ => return Err("a reference requires a non-empty `id` or `lid`".to_owned()),
    }
    if let Some(relationship) = &reference.relationship {
        if relationship.is_empty() {
            return Err("reference `relationship` must not be empty".to_owned());
        }
        definition
            .relationship_by_name(relationship)
            .ok_or_else(|| {
                format!(
                    "relationship `{relationship}` is not registered on `{}`",
                    reference.type_name
                )
            })?;
    }
    Ok(())
}

fn parse_resource_data(
    operation: &AtomicOperation,
    index: usize,
    path: &str,
) -> Result<AtomicResourceData, AtomicOperationsError> {
    let value = operation
        .data
        .as_ref()
        .ok_or_else(|| invalid_operation(index, path, "resource operation requires `data`"))?;
    serde_json::from_value(value.clone()).map_err(|error| {
        invalid_operation(
            index,
            path,
            &format!("resource `data` is malformed: {error}"),
        )
    })
}

fn parse_relationship_data(
    operation: &AtomicOperation,
    index: usize,
    path: &str,
) -> Result<RelationshipData, AtomicOperationsError> {
    let value = operation
        .data
        .as_ref()
        .ok_or_else(|| invalid_operation(index, path, "relationship operation requires `data`"))?;
    serde_json::from_value(value.clone()).map_err(|error| {
        invalid_operation(
            index,
            path,
            &format!("relationship `data` is malformed: {error}"),
        )
    })
}

fn validate_resource_data(
    registry: &ResourceRegistry,
    data: &AtomicResourceData,
    local_ids: &BTreeSet<(String, String)>,
    index: usize,
    path: &str,
    is_add: bool,
) -> Result<AtomicResourceChangeset, AtomicOperationsError> {
    let fail = |message: String| invalid_operation(index, path, &message);
    if data.type_name.is_empty() {
        return Err(fail("resource `type` must not be empty".to_owned()));
    }
    if data.id.as_deref().is_some_and(str::is_empty)
        || data.lid.as_deref().is_some_and(str::is_empty)
    {
        return Err(fail("resource identifiers must not be empty".to_owned()));
    }
    if data.id.is_some() && data.lid.is_some() {
        return Err(fail(
            "resource data must not contain both `id` and `lid`".to_owned(),
        ));
    }
    if data
        .lid
        .as_ref()
        .is_some_and(|lid| !is_add && !local_ids.contains(&(data.type_name.clone(), lid.clone())))
    {
        return Err(fail(format!(
            "resource local id `{}` has not been added earlier",
            data.lid.as_deref().unwrap_or_default()
        )));
    }
    let definition = registry.resource(&data.type_name).map_err(|_| {
        fail(format!(
            "resource type `{}` is not registered",
            data.type_name
        ))
    })?;
    let mut mapped_attributes = None;
    if let Some(attributes) = &data.attributes {
        let mut mapped = BTreeMap::new();
        for (name, value) in attributes {
            let mapping = definition.attribute_by_name(name).ok_or_else(|| {
                fail(format!(
                    "attribute `{name}` is not registered on `{}`",
                    data.type_name
                ))
            })?;
            mapped.insert(mapping.model_field().to_owned(), value.clone());
        }
        mapped_attributes = Some(mapped);
    }
    let mut mapped_relationships = None;
    if let Some(relationships) = &data.relationships {
        let mut mapped = BTreeMap::new();
        for (name, relationship) in relationships {
            let mapping = definition.relationship_by_name(name).ok_or_else(|| {
                fail(format!(
                    "relationship `{name}` is not registered on `{}`",
                    data.type_name
                ))
            })?;
            if let Some(linkage) = &relationship.data {
                validate_relationship_data(
                    registry,
                    mapping.target_type(),
                    linkage,
                    local_ids,
                    index,
                    path,
                )?;
            }
            mapped.insert(
                mapping.model_field().to_owned(),
                MappedRelationshipChange {
                    data: relationship.data.clone(),
                },
            );
        }
        mapped_relationships = Some(mapped);
    }
    Ok(AtomicResourceChangeset {
        type_name: data.type_name.clone(),
        identifier_field: definition.identifier_field().to_owned(),
        id: data.id.clone(),
        lid: data.lid.clone(),
        attributes: mapped_attributes,
        relationships: mapped_relationships,
    })
}

fn validate_relationship_data(
    registry: &ResourceRegistry,
    target_type: &str,
    data: &RelationshipData,
    local_ids: &BTreeSet<(String, String)>,
    index: usize,
    path: &str,
) -> Result<(), AtomicOperationsError> {
    match data {
        RelationshipData::Null => Ok(()),
        RelationshipData::One(identifier) => validate_linkage(
            registry,
            target_type,
            std::slice::from_ref(identifier),
            local_ids,
            index,
            path,
        ),
        RelationshipData::Many(identifiers) => {
            validate_linkage(registry, target_type, identifiers, local_ids, index, path)
        }
    }
}

fn validate_linkage(
    registry: &ResourceRegistry,
    target_type: &str,
    identifiers: &[ResourceIdentifier],
    local_ids: &BTreeSet<(String, String)>,
    index: usize,
    path: &str,
) -> Result<(), AtomicOperationsError> {
    for identifier in identifiers {
        if identifier.type_name != target_type {
            return Err(invalid_operation(
                index,
                path,
                &format!(
                    "relationship linkage type `{}` does not match target `{target_type}`",
                    identifier.type_name
                ),
            ));
        }
        registry.resource(&identifier.type_name).map_err(|_| {
            invalid_operation(
                index,
                path,
                &format!("resource type `{}` is not registered", identifier.type_name),
            )
        })?;
        if identifier.type_name.is_empty()
            || identifier.id.as_deref().is_some_and(str::is_empty)
            || identifier.lid.as_deref().is_some_and(str::is_empty)
            || (identifier.id.is_none() && identifier.lid.is_none())
            || (identifier.id.is_some() && identifier.lid.is_some())
        {
            return Err(invalid_operation(
                index,
                path,
                "relationship identifiers require exactly one non-empty `id` or `lid`",
            ));
        }
        if let Some(lid) = &identifier.lid {
            if !local_ids.contains(&(identifier.type_name.clone(), lid.clone())) {
                return Err(invalid_operation(
                    index,
                    path,
                    &format!(
                        "relationship local id `{lid}` for `{}` has not been added earlier",
                        identifier.type_name
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn insert_local_id(
    local_ids: &mut BTreeSet<(String, String)>,
    type_name: &str,
    lid: &str,
    index: usize,
    path: &str,
) -> Result<(), AtomicOperationsError> {
    if !local_ids.insert((type_name.to_owned(), lid.to_owned())) {
        return Err(invalid_operation(
            index,
            path,
            &format!("local id `{lid}` is already declared for `{type_name}`"),
        ));
    }
    Ok(())
}

fn resource_target(
    operation: &AtomicOperation,
    data: &AtomicResourceData,
    index: usize,
    path: &str,
) -> Result<AtomicTarget, AtomicOperationsError> {
    if let Some(reference) = &operation.reference {
        Ok(AtomicTarget::Reference(reference.clone()))
    } else if let Some(href) = &operation.href {
        Ok(AtomicTarget::Href(href.clone()))
    } else if let Some(id) = &data.id {
        Ok(AtomicTarget::Reference(AtomicResourceReference {
            type_name: data.type_name.clone(),
            id: Some(id.clone()),
            lid: None,
            relationship: None,
        }))
    } else if let Some(lid) = &data.lid {
        Ok(AtomicTarget::Reference(AtomicResourceReference {
            type_name: data.type_name.clone(),
            id: None,
            lid: Some(lid.clone()),
            relationship: None,
        }))
    } else {
        Err(invalid_operation(
            index,
            path,
            "update requires `ref`, `href`, or resource `id`/`lid`",
        ))
    }
}

fn operation_target(operation: &AtomicOperation) -> Option<AtomicTarget> {
    if let Some(reference) = &operation.reference {
        Some(AtomicTarget::Reference(reference.clone()))
    } else {
        operation
            .href
            .as_ref()
            .map(|href| AtomicTarget::Href(href.clone()))
    }
}

fn invalid_operation(index: usize, path: &str, message: &str) -> AtomicOperationsError {
    AtomicOperationsError::InvalidOperation {
        index,
        pointer: path.to_owned(),
        message: message.to_owned(),
    }
}

/// A request result plus an optional created identity for local-ID resolution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AtomicOperationOutcome {
    /// The positional result object.
    pub result: AtomicResult,
    /// The persistent identity assigned by an add-resource operation.
    pub created_resource: Option<ResourceIdentifier>,
}

/// Request-local mappings from added `lid` values to persistent identifiers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalIdMap {
    identities: BTreeMap<(String, String), ResourceIdentifier>,
}

impl LocalIdMap {
    /// Resolves an identifier, replacing a previously registered local ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the local ID has not been assigned yet.
    pub fn resolve(&self, identifier: &ResourceIdentifier) -> Result<ResourceIdentifier, String> {
        match (&identifier.id, &identifier.lid) {
            (Some(id), None) if !identifier.type_name.is_empty() && !id.is_empty() => {
                Ok(identifier.clone())
            }
            (None, Some(lid)) => self
                .identities
                .get(&(identifier.type_name.clone(), lid.clone()))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "local id `{lid}` for `{}` has not been assigned",
                        identifier.type_name
                    )
                }),
            _ => Err("an identifier must contain exactly one of `id` or `lid`".to_owned()),
        }
    }

    /// Resolves an operation reference and preserves its relationship target.
    ///
    /// # Errors
    ///
    /// Returns an error when the local ID has not been assigned yet.
    pub fn resolve_reference(
        &self,
        reference: &AtomicResourceReference,
    ) -> Result<AtomicResourceReference, String> {
        let identity = self.resolve(&ResourceIdentifier {
            type_name: reference.type_name.clone(),
            id: reference.id.clone(),
            lid: reference.lid.clone(),
            ..ResourceIdentifier::default()
        })?;
        Ok(AtomicResourceReference {
            type_name: identity.type_name,
            id: identity.id,
            lid: None,
            relationship: reference.relationship.clone(),
        })
    }

    fn insert(
        &mut self,
        type_name: &str,
        lid: &str,
        identity: ResourceIdentifier,
    ) -> Result<(), String> {
        if identity.type_name != type_name
            || identity.id.as_deref().is_none_or(str::is_empty)
            || identity.lid.is_some()
        {
            return Err(format!(
                "add operation for `{type_name}` lid `{lid}` did not return a persistent identity"
            ));
        }
        if self
            .identities
            .insert((type_name.to_owned(), lid.to_owned()), identity)
            .is_some()
        {
            return Err(format!(
                "local id `{lid}` for `{type_name}` was assigned more than once"
            ));
        }
        Ok(())
    }
}

/// Application-defined execution of one planned operation.
#[async_trait]
pub trait AtomicOperationHandler: Send + Sync {
    /// Executes one operation using the executor's shared transaction.
    ///
    /// The handler must resolve any `lid` values through `local_ids` before
    /// writing relationship linkage. It returns the assigned persistent
    /// identity for an add operation that declared a resource `lid`.
    ///
    /// # Errors
    ///
    /// Returns an application error to roll back the entire operations request.
    async fn execute_operation(
        &self,
        transaction: &DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String>;
}

/// Authorizes and bounds a planned Atomic Operations request before transaction start.
#[async_trait]
pub trait AtomicOperationsGuard: Send + Sync {
    /// Returns whether the caller may execute every operation in the request.
    async fn authorize(&self, headers: &HeaderMap, operations: &[PlannedAtomicOperation]) -> bool;

    /// Checks operation count and any application-specific request limits.
    ///
    /// # Errors
    ///
    /// Returns a description when the request exceeds an application limit.
    fn validate_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String>;
}

/// A failure while executing an Atomic Operations transaction.
#[derive(Debug)]
pub enum AtomicExecutionError {
    /// The request was not authorized.
    NotAuthorized,
    /// The guard rejected the request for exceeding application limits.
    LimitExceeded(String),
    /// Starting or committing the transaction failed.
    Database(DbErr),
    /// An operation failed; its transaction has been rolled back.
    Operation {
        /// Zero-based index of the failed operation.
        index: usize,
        /// The application handler's error.
        message: String,
    },
    /// Rolling back after an operation error also failed.
    Rollback {
        /// Zero-based index of the failed operation.
        index: usize,
        /// The original operation failure.
        operation: String,
        /// The rollback failure.
        rollback: DbErr,
    },
    /// A created local ID could not be mapped to its persistent identity.
    LocalId {
        /// Zero-based index of the add operation.
        index: usize,
        /// The local-ID validation failure.
        message: String,
    },
    /// An operation produced a result that violates the extension contract.
    InvalidResult {
        /// Zero-based index of the operation.
        index: usize,
        /// The result validation failure.
        message: String,
    },
}

impl fmt::Display for AtomicExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAuthorized => {
                formatter.write_str("Atomic Operations request is not authorized")
            }
            Self::LimitExceeded(message) => {
                write!(
                    formatter,
                    "Atomic Operations request exceeds limits: {message}"
                )
            }
            Self::Database(error) => write!(formatter, "atomic transaction failed: {error}"),
            Self::Operation { index, message } => {
                write!(formatter, "operation {index} failed: {message}")
            }
            Self::Rollback {
                index,
                operation,
                rollback,
            } => write!(
                formatter,
                "operation {index} failed: {operation}; rollback also failed: {rollback}"
            ),
            Self::LocalId { index, message } => {
                write!(formatter, "operation {index} local ID failed: {message}")
            }
            Self::InvalidResult { index, message } => {
                write!(
                    formatter,
                    "operation {index} returned an invalid result: {message}"
                )
            }
        }
    }
}

impl std::error::Error for AtomicExecutionError {}

/// Executes operations in order inside one SeaORM transaction.
///
/// Any handler failure or local-ID mapping failure rolls back prior writes.
/// Successful results are returned in request order and are committed only
/// after every operation succeeds.
///
/// # Errors
///
/// Returns a database error for begin/commit failures, or the failed operation
/// and rollback error if an operation cannot be completed atomically.
pub async fn execute_atomic_operations<H>(
    database: &DatabaseConnection,
    operations: &[PlannedAtomicOperation],
    headers: &HeaderMap,
    guard: &dyn AtomicOperationsGuard,
    handler: &H,
) -> Result<Vec<AtomicResult>, AtomicExecutionError>
where
    H: AtomicOperationHandler + ?Sized,
{
    if !guard.authorize(headers, operations).await {
        return Err(AtomicExecutionError::NotAuthorized);
    }
    guard
        .validate_limits(operations)
        .map_err(AtomicExecutionError::LimitExceeded)?;
    if operations.is_empty() {
        return Ok(Vec::new());
    }
    let transaction = database
        .begin()
        .await
        .map_err(AtomicExecutionError::Database)?;
    let mut local_ids = LocalIdMap::default();
    let mut results = Vec::with_capacity(operations.len());

    for (index, operation) in operations.iter().enumerate() {
        let outcome = match handler
            .execute_operation(&transaction, &operation.operation, &local_ids)
            .await
        {
            Ok(outcome) => outcome,
            Err(message) => {
                return match transaction.rollback().await {
                    Ok(()) => Err(AtomicExecutionError::Operation { index, message }),
                    Err(rollback) => Err(AtomicExecutionError::Rollback {
                        index,
                        operation: message,
                        rollback,
                    }),
                };
            }
        };

        if let Err(message) = validate_operation_result(&operation.operation, &outcome.result) {
            return match transaction.rollback().await {
                Ok(()) => Err(AtomicExecutionError::InvalidResult { index, message }),
                Err(rollback) => Err(AtomicExecutionError::Rollback {
                    index,
                    operation: message,
                    rollback,
                }),
            };
        }

        if let PlannedOperation::AddResource { data, .. } = &operation.operation {
            if let Some(lid) = &data.lid {
                let Some(identity) = outcome.created_resource else {
                    return match transaction.rollback().await {
                        Ok(()) => Err(AtomicExecutionError::LocalId {
                            index,
                            message: format!(
                                "add operation did not return an identity for lid `{lid}`"
                            ),
                        }),
                        Err(rollback) => Err(AtomicExecutionError::Rollback {
                            index,
                            operation: format!(
                                "add operation did not return an identity for lid `{lid}`"
                            ),
                            rollback,
                        }),
                    };
                };
                if let Some(result_data) = &outcome.result.data {
                    let returned = match resource_result_identity(&data.type_name, result_data) {
                        Ok(returned) => returned,
                        Err(message) => {
                            return match transaction.rollback().await {
                                Ok(()) => {
                                    Err(AtomicExecutionError::InvalidResult { index, message })
                                }
                                Err(rollback) => Err(AtomicExecutionError::Rollback {
                                    index,
                                    operation: message,
                                    rollback,
                                }),
                            };
                        }
                    };
                    if identity.type_name != returned.type_name || identity.id != returned.id {
                        let message = "created local-ID mapping does not match the resource result"
                            .to_owned();
                        return match transaction.rollback().await {
                            Ok(()) => Err(AtomicExecutionError::InvalidResult { index, message }),
                            Err(rollback) => Err(AtomicExecutionError::Rollback {
                                index,
                                operation: message,
                                rollback,
                            }),
                        };
                    }
                }
                if let Err(message) = local_ids.insert(&data.type_name, lid, identity) {
                    return match transaction.rollback().await {
                        Ok(()) => Err(AtomicExecutionError::LocalId { index, message }),
                        Err(rollback) => Err(AtomicExecutionError::Rollback {
                            index,
                            operation: message,
                            rollback,
                        }),
                    };
                }
            }
        }
        results.push(outcome.result);
    }

    transaction
        .commit()
        .await
        .map_err(AtomicExecutionError::Database)?;
    Ok(results)
}

fn validate_operation_result(
    operation: &PlannedOperation,
    result: &AtomicResult,
) -> Result<(), String> {
    match operation {
        PlannedOperation::AddResource { data, .. } => {
            if data.id.is_none() && result.data.is_none() {
                return Err(
                    "a server-assigned resource ID requires a resource representation".to_owned(),
                );
            }
            if let Some(result_data) = &result.data {
                let identifier = resource_result_identity(&data.type_name, result_data)?;
                if data
                    .id
                    .as_ref()
                    .is_some_and(|requested| identifier.id.as_ref() != Some(requested))
                {
                    return Err("resource result ID must match the requested ID".to_owned());
                }
            }
        }
        PlannedOperation::UpdateResource { target, data, .. } => {
            if let Some(result_data) = &result.data {
                let expected_type = match target {
                    AtomicTarget::Reference(reference) => reference.type_name.as_str(),
                    AtomicTarget::Href(_) => data.type_name.as_str(),
                };
                resource_result_identity(expected_type, result_data)?;
            }
        }
        PlannedOperation::AddRelationshipMembers { .. }
        | PlannedOperation::UpdateRelationship { .. }
        | PlannedOperation::RemoveResource { .. }
        | PlannedOperation::RemoveRelationshipMembers { .. } => {
            if result.data.is_some() {
                return Err("this operation result must not contain `data`".to_owned());
            }
        }
    }
    Ok(())
}

fn resource_result_identity(
    expected_type: &str,
    data: &Value,
) -> Result<ResourceIdentifier, String> {
    let identifier: ResourceIdentifier = serde_json::from_value(data.clone())
        .map_err(|error| format!("resource result data is malformed: {error}"))?;
    if identifier.type_name != expected_type
        || identifier.id.as_deref().is_none_or(str::is_empty)
        || identifier.lid.is_some()
    {
        return Err(format!(
            "resource result data must contain a persistent `{expected_type}` identity"
        ));
    }
    Ok(identifier)
}

fn deserialize_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
