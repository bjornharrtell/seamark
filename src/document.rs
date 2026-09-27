//! JSON:API document types and validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;

use language_tags::LanguageTag;
use serde::de::value::MapAccessDeserializer;
use serde::de::{DeserializeSeed, Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use uriparse::URIReference;

/// A JSON:API top-level document.
///
/// The document may contain primary data, errors, or metadata. This type
/// validates those mutually exclusive top-level forms, but does not claim to
/// validate every JSON:API 1.1 requirement.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<JsonApiDocumentRepr>")]
pub struct JsonApiDocument {
    /// The primary resource data, including an explicit JSON `null`.
    #[serde(
        default,
        deserialize_with = "deserialize_primary_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<PrimaryData>,
    /// Errors describing a failed request.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub errors: Option<Vec<ErrorObject>>,
    /// Resources included to represent compound documents.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub included: Option<Vec<ResourceObject>>,
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
    /// JSON:API version and supported extension/profile information.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub jsonapi: Option<JsonApiObject>,
}

#[derive(Deserialize)]
struct JsonApiDocumentRepr {
    #[serde(
        default,
        deserialize_with = "deserialize_primary_data",
        skip_serializing_if = "Option::is_none"
    )]
    data: Option<PrimaryData>,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    errors: Option<Vec<ErrorObject>>,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    included: Option<Vec<ResourceObject>>,
    #[serde(
        default,
        deserialize_with = "deserialize_links",
        skip_serializing_if = "Option::is_none"
    )]
    links: Option<Map<String, Value>>,
    #[serde(
        default,
        deserialize_with = "deserialize_metadata",
        skip_serializing_if = "Option::is_none"
    )]
    meta: Option<Map<String, Value>>,
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    jsonapi: Option<JsonApiObject>,
}

impl From<ObjectOnly<JsonApiDocumentRepr>> for JsonApiDocument {
    fn from(ObjectOnly(document): ObjectOnly<JsonApiDocumentRepr>) -> Self {
        Self {
            data: document.data,
            errors: document.errors,
            included: document.included,
            links: document.links,
            meta: document.meta,
            jsonapi: document.jsonapi,
        }
    }
}

/// Restricts protocol objects to map representations rather than Serde's sequence form.
pub(crate) struct ObjectOnly<T>(T);

impl<T> ObjectOnly<T> {
    pub(crate) fn into_inner(self) -> T {
        self.0
    }
}

impl<'de, T> Deserialize<'de> for ObjectOnly<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T> Visitor<'de> for ObjectVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = ObjectOnly<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                T::deserialize(MapAccessDeserializer::new(ValidatingMapAccess(map))).map(ObjectOnly)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

struct ValidatingMapAccess<M>(M);

struct ValidatingKeySeed<S>(S);

impl<'de, S> DeserializeSeed<'de> for ValidatingKeySeed<S>
where
    S: DeserializeSeed<'de>,
{
    type Value = S::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let key = String::deserialize(deserializer)?;
        if is_at_member(&key) {
            validate_member_name(&key).map_err(D::Error::custom)?;
        }
        self.0
            .deserialize(serde::de::value::StringDeserializer::<D::Error>::new(key))
    }
}

impl<'de, M> MapAccess<'de> for ValidatingMapAccess<M>
where
    M: MapAccess<'de>,
{
    type Error = M::Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        self.0.next_key_seed(ValidatingKeySeed(seed))
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        self.0.next_value_seed(seed)
    }

    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}

impl JsonApiDocument {
    /// Validates the document's top-level structure and resource identifiers.
    ///
    /// This is a structural check, not a complete conformance validator.
    /// Included-resource reachability and selected link and JSON:API object
    /// semantics are checked, but several context-dependent protocol rules
    /// are not.
    ///
    /// # Errors
    ///
    /// Returns an error when required top-level content is absent, `data` and
    /// `errors` coexist, the errors array is empty, included resources have no
    /// primary data, included resources are unreachable from primary data, a
    /// resource identity is duplicated, or a resource/identifier, link, or
    /// JSON:API capability URI is invalid.
    pub fn validate(&self) -> Result<(), DocumentValidationError> {
        self.validate_with_included_reachability(true)
    }

    fn validate_with_included_reachability(
        &self,
        require_included_reachability: bool,
    ) -> Result<(), DocumentValidationError> {
        validate_links(self.links.as_ref())?;
        if let Some(jsonapi) = &self.jsonapi {
            jsonapi.validate()?;
        }
        if self.data.is_some() && self.errors.is_some() {
            return Err(DocumentValidationError::DataAndErrors);
        }
        if self.included.is_some() && self.data.is_none() {
            return Err(DocumentValidationError::IncludedWithoutData);
        }
        if self.data.is_none() && self.errors.is_none() && self.meta.is_none() {
            return Err(DocumentValidationError::MissingContent);
        }
        if self.errors.as_ref().is_some_and(Vec::is_empty) {
            return Err(DocumentValidationError::EmptyErrors);
        }
        if let Some(errors) = &self.errors {
            for error in errors {
                error.validate()?;
            }
        }

        let mut identities = HashSet::new();
        match &self.data {
            Some(PrimaryData::One(resource)) => {
                resource.validate()?;
                track_resource_identity(resource, &mut identities)?;
            }
            Some(PrimaryData::Many(resources)) => {
                for resource in resources {
                    resource.validate()?;
                    track_resource_identity(resource, &mut identities)?;
                }
            }
            Some(PrimaryData::Null) | None => {}
        }

        if let Some(included) = &self.included {
            for resource in included {
                resource.validate()?;
                track_resource_identity(resource, &mut identities)?;
            }
        }
        validate_relationship_local_ids(
            self.data.as_ref(),
            self.included.as_deref().unwrap_or_default(),
            &identities,
        )?;
        if require_included_reachability {
            if let Some(included) = &self.included {
                validate_included_reachability(self.data.as_ref(), included)?;
            }
        }

        Ok(())
    }

    /// Validates the document for use as a JSON:API response.
    ///
    /// Unlike the context-neutral [`validate`](Self::validate), a response
    /// requires every resource object to have a persistent `id`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`validate`](Self::validate), or
    /// [`MissingResourceId`](DocumentValidationError::MissingResourceId) when
    /// a primary or included resource has no `id`.
    pub fn validate_response(&self) -> Result<(), DocumentValidationError> {
        self.validate()?;
        self.validate_response_resource_ids()
    }

    /// Validates a response when a requested sparse fieldset excludes include linkage.
    ///
    /// The caller must establish that the sparse fieldset excludes a relationship
    /// from the validated include plan.
    pub(crate) fn validate_response_with_sparse_fieldset_exception(
        &self,
    ) -> Result<(), DocumentValidationError> {
        self.validate_with_included_reachability(false)?;
        self.validate_response_resource_ids()
    }

    fn validate_response_resource_ids(&self) -> Result<(), DocumentValidationError> {
        if let Some(PrimaryData::One(resource)) = &self.data {
            validate_response_resource_id(resource)?;
            validate_response_relationship_identifiers(resource)?;
        }
        if let Some(PrimaryData::Many(resources)) = &self.data {
            for resource in resources {
                validate_response_resource_id(resource)?;
                validate_response_relationship_identifiers(resource)?;
            }
        }
        if let Some(included) = &self.included {
            for resource in included {
                validate_response_resource_id(resource)?;
                validate_response_relationship_identifiers(resource)?;
            }
        }
        Ok(())
    }
}

/// Primary data in a JSON:API document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrimaryData {
    /// Explicit JSON `null`, commonly used for an empty to-one relationship.
    Null,
    /// One primary resource.
    One(ResourceObject),
    /// A collection of primary resources.
    Many(Vec<ResourceObject>),
}

/// A JSON:API resource object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<ResourceObjectRepr>")]
pub struct ResourceObject {
    /// The resource type.
    #[serde(rename = "type")]
    pub type_name: String,
    /// A persistent resource identifier.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// A request-local identifier for a resource not yet assigned an `id`.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub lid: Option<String>,
    /// Resource attributes keyed by public attribute name.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub attributes: Option<Map<String, Value>>,
    /// Resource relationships keyed by public relationship name.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub relationships: Option<BTreeMap<String, Relationship>>,
    /// Links related to this resource.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub links: Option<Map<String, Value>>,
    /// Non-standard resource metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct ResourceObjectRepr {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    lid: Option<String>,
    #[serde(default, deserialize_with = "deserialize_attributes")]
    attributes: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_relationships")]
    relationships: Option<BTreeMap<String, Relationship>>,
    #[serde(default, deserialize_with = "deserialize_links")]
    links: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    meta: Option<Map<String, Value>>,
}

impl From<ObjectOnly<ResourceObjectRepr>> for ResourceObject {
    fn from(ObjectOnly(resource): ObjectOnly<ResourceObjectRepr>) -> Self {
        Self {
            type_name: resource.type_name,
            id: resource.id,
            lid: resource.lid,
            attributes: resource.attributes,
            relationships: resource.relationships,
            links: resource.links,
            meta: resource.meta,
        }
    }
}

impl ResourceObject {
    fn validate(&self) -> Result<(), DocumentValidationError> {
        validate_type(&self.type_name)?;
        match (&self.id, &self.lid) {
            (None, None) => return Err(DocumentValidationError::MissingIdentifier),
            (Some(_), Some(_)) => return Err(DocumentValidationError::BothIdentifiers),
            _ => {}
        }
        if let Some(attributes) = &self.attributes {
            for name in attributes.keys() {
                validate_member_name(name)?;
                if is_at_member(name) {
                    continue;
                }
                if name == "type"
                    || name == "id"
                    || self
                        .relationships
                        .as_ref()
                        .is_some_and(|relationships| relationships.contains_key(name))
                {
                    return Err(DocumentValidationError::ConflictingFieldName);
                }
            }
        }
        validate_links(self.links.as_ref())?;
        if let Some(relationships) = &self.relationships {
            for (name, relationship) in relationships {
                if is_at_member(name) {
                    continue;
                }
                validate_member_name(name)?;
                if name == "type" || name == "id" {
                    return Err(DocumentValidationError::ConflictingFieldName);
                }
                relationship.validate()?;
            }
        }
        Ok(())
    }
}

/// A relationship object, containing linkage, links, or metadata.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<RelationshipRepr>")]
pub struct Relationship {
    /// Resource linkage, including explicit JSON `null`.
    #[serde(
        default,
        deserialize_with = "deserialize_relationship_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<RelationshipData>,
    /// Links related to this relationship.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub links: Option<Map<String, Value>>,
    /// Non-standard relationship metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct RelationshipRepr {
    #[serde(default, deserialize_with = "deserialize_relationship_data")]
    data: Option<RelationshipData>,
    #[serde(default, deserialize_with = "deserialize_links")]
    links: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    meta: Option<Map<String, Value>>,
}

impl From<ObjectOnly<RelationshipRepr>> for Relationship {
    fn from(ObjectOnly(relationship): ObjectOnly<RelationshipRepr>) -> Self {
        Self {
            data: relationship.data,
            links: relationship.links,
            meta: relationship.meta,
        }
    }
}

impl Relationship {
    pub(crate) fn validate(&self) -> Result<(), DocumentValidationError> {
        let has_links = self
            .links
            .as_ref()
            .is_some_and(|links| links.keys().any(|relation| !is_at_member(relation)));
        if self.data.is_none() && !has_links && self.meta.is_none() {
            return Err(DocumentValidationError::EmptyRelationship);
        }
        validate_links(self.links.as_ref())?;
        if let Some(data) = &self.data {
            match data {
                RelationshipData::Null => {}
                RelationshipData::One(identifier) => identifier.validate()?,
                RelationshipData::Many(identifiers) => {
                    for identifier in identifiers {
                        identifier.validate()?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Resource linkage in a relationship.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RelationshipData {
    /// Explicit JSON `null` for an empty to-one relationship.
    Null,
    /// One resource identifier.
    One(ResourceIdentifier),
    /// A collection of resource identifiers.
    Many(Vec<ResourceIdentifier>),
}

/// A JSON:API resource identifier object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<ResourceIdentifierRepr>")]
pub struct ResourceIdentifier {
    /// The resource type.
    #[serde(rename = "type")]
    pub type_name: String,
    /// A persistent resource identifier.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// A request-local resource identifier.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub lid: Option<String>,
    /// Non-standard identifier metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct ResourceIdentifierRepr {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    lid: Option<String>,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    meta: Option<Map<String, Value>>,
}

impl From<ObjectOnly<ResourceIdentifierRepr>> for ResourceIdentifier {
    fn from(ObjectOnly(identifier): ObjectOnly<ResourceIdentifierRepr>) -> Self {
        Self {
            type_name: identifier.type_name,
            id: identifier.id,
            lid: identifier.lid,
            meta: identifier.meta,
        }
    }
}

impl ResourceIdentifier {
    fn validate(&self) -> Result<(), DocumentValidationError> {
        validate_type(&self.type_name)?;
        match (&self.id, &self.lid) {
            (None, None) => return Err(DocumentValidationError::MissingIdentifier),
            (Some(_), Some(_)) => return Err(DocumentValidationError::BothIdentifiers),
            _ => {}
        }
        Ok(())
    }
}

/// A JSON:API error object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<ErrorObjectRepr>")]
pub struct ErrorObject {
    /// A unique identifier for this particular occurrence of the problem.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
    /// Links to additional information about the error.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub links: Option<Map<String, Value>>,
    /// The HTTP status code as a string.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<String>,
    /// An application-specific error code.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub code: Option<String>,
    /// A short, human-readable summary of the problem.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<String>,
    /// A human-readable explanation specific to this occurrence.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
    /// Information about the request source associated with this error.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub source: Option<ErrorSource>,
    /// Non-standard error metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct ErrorObjectRepr {
    #[serde(default, deserialize_with = "deserialize_non_null")]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_links")]
    links: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    code: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    detail: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    source: Option<ErrorSource>,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    meta: Option<Map<String, Value>>,
}

impl From<ObjectOnly<ErrorObjectRepr>> for ErrorObject {
    fn from(ObjectOnly(error): ObjectOnly<ErrorObjectRepr>) -> Self {
        Self {
            id: error.id,
            links: error.links,
            status: error.status,
            code: error.code,
            title: error.title,
            detail: error.detail,
            source: error.source,
            meta: error.meta,
        }
    }
}

impl ErrorObject {
    fn validate(&self) -> Result<(), DocumentValidationError> {
        if self.id.is_none()
            && self.links.is_none()
            && self.status.is_none()
            && self.code.is_none()
            && self.title.is_none()
            && self.detail.is_none()
            && self.source.is_none()
            && self.meta.is_none()
        {
            return Err(DocumentValidationError::EmptyErrorObject);
        }
        validate_links(self.links.as_ref())?;
        if self
            .status
            .as_deref()
            .is_some_and(|status| !is_valid_http_status(status))
        {
            return Err(DocumentValidationError::InvalidErrorStatus);
        }
        if self
            .source
            .as_ref()
            .and_then(|source| source.pointer.as_deref())
            .is_some_and(|pointer| !is_valid_json_pointer(pointer))
        {
            return Err(DocumentValidationError::InvalidErrorSourcePointer);
        }
        Ok(())
    }
}

/// Source information identifying the request portion associated with an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<ErrorSourceRepr>")]
pub struct ErrorSource {
    /// A JSON Pointer to the request document location.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub pointer: Option<String>,
    /// The name of a query parameter.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub parameter: Option<String>,
    /// The name of a request header.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub header: Option<String>,
}

#[derive(Deserialize)]
struct ErrorSourceRepr {
    #[serde(default, deserialize_with = "deserialize_non_null")]
    pointer: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    parameter: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    header: Option<String>,
}

impl From<ObjectOnly<ErrorSourceRepr>> for ErrorSource {
    fn from(ObjectOnly(source): ObjectOnly<ErrorSourceRepr>) -> Self {
        Self {
            pointer: source.pointer,
            parameter: source.parameter,
            header: source.header,
        }
    }
}

/// JSON:API version and capability information.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ObjectOnly<JsonApiObjectRepr>")]
pub struct JsonApiObject {
    /// The JSON:API version string.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub version: Option<String>,
    /// Supported extension URIs.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub ext: Option<Vec<String>>,
    /// Applied profile URIs.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<Vec<String>>,
    /// Non-standard JSON:API metadata.
    #[serde(
        default,
        deserialize_with = "deserialize_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct JsonApiObjectRepr {
    #[serde(default, deserialize_with = "deserialize_non_null")]
    version: Option<String>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    ext: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_non_null")]
    profile: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    meta: Option<Map<String, Value>>,
}

impl From<ObjectOnly<JsonApiObjectRepr>> for JsonApiObject {
    fn from(ObjectOnly(jsonapi): ObjectOnly<JsonApiObjectRepr>) -> Self {
        Self {
            version: jsonapi.version,
            ext: jsonapi.ext,
            profile: jsonapi.profile,
            meta: jsonapi.meta,
        }
    }
}

impl JsonApiObject {
    fn validate(&self) -> Result<(), DocumentValidationError> {
        for uri in self.ext.iter().chain(self.profile.iter()).flatten() {
            if !is_valid_absolute_uri(uri) {
                return Err(DocumentValidationError::InvalidJsonApiUri);
            }
        }
        Ok(())
    }
}

/// A structural JSON:API document validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentValidationError {
    /// The document has neither primary data, errors, nor metadata.
    MissingContent,
    /// The document contains both primary data and errors.
    DataAndErrors,
    /// The document contains an empty errors array.
    EmptyErrors,
    /// An error object has none of its defined members.
    EmptyErrorObject,
    /// A links object contains an invalid link, URI-reference, or relation type.
    InvalidLinkObject,
    /// The JSON:API object contains a relative or malformed capability URI.
    InvalidJsonApiUri,
    /// An error object contains an invalid HTTP status code.
    InvalidErrorStatus,
    /// An error source contains an invalid JSON Pointer.
    InvalidErrorSourcePointer,
    /// The document contains included resources without primary data.
    IncludedWithoutData,
    /// A resource or identifier has an empty type.
    EmptyType,
    /// A resource object or identifier has neither an `id` nor a `lid`.
    MissingIdentifier,
    /// A resource object or identifier has both an `id` and a `lid`.
    BothIdentifiers,
    /// A resource type or field name violates JSON:API member-name rules.
    InvalidMemberName,
    /// A resource has fields that conflict with its `type` or `id`, or each other.
    ConflictingFieldName,
    /// A relationship object has no linkage, a non-empty links object, or metadata.
    EmptyRelationship,
    /// An included resource cannot be reached through primary resource linkage.
    UnreachableIncludedResource,
    /// A relationship refers to a local identifier absent from the document.
    UnresolvedLocalIdentifier,
    /// A response resource object has no persistent `id`.
    MissingResourceId,
    /// A response relationship identifier has no persistent `id`.
    MissingResponseIdentifierId,
    /// A resource object is repeated in primary or included resource data.
    DuplicateResourceIdentifier,
}

impl fmt::Display for DocumentValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingContent => "a JSON:API document must contain data, errors, or meta",
            Self::DataAndErrors => "a JSON:API document must not contain both data and errors",
            Self::EmptyErrors => "a JSON:API errors array must not be empty",
            Self::EmptyErrorObject => "a JSON:API error object must contain at least one member",
            Self::InvalidLinkObject => {
                "each links member must use a valid relation type and a valid URI-reference href"
            }
            Self::InvalidJsonApiUri => {
                "JSON:API extension and profile members must contain absolute URIs"
            }
            Self::InvalidErrorStatus => {
                "an error status must be an HTTP status code from 100 through 599"
            }
            Self::InvalidErrorSourcePointer => {
                "an error source pointer must use valid JSON Pointer syntax"
            }
            Self::IncludedWithoutData => {
                "a JSON:API document must not contain included resources without data"
            }
            Self::EmptyType => "a resource type must not be empty",
            Self::MissingIdentifier => "a resource object or identifier must contain an id or lid",
            Self::BothIdentifiers => {
                "a resource object or identifier must not contain both id and lid"
            }
            Self::InvalidMemberName => "a resource type or field name is not a valid member name",
            Self::ConflictingFieldName => {
                "resource field names must not conflict with type, id, or each other"
            }
            Self::EmptyRelationship => {
                "a relationship object must contain data, non-empty links, or meta"
            }
            Self::UnreachableIncludedResource => {
                "every included resource must be reachable from primary data through relationship linkage"
            }
            Self::UnresolvedLocalIdentifier => {
                "a relationship local identifier must match a resource object in the document"
            }
            Self::MissingResourceId => "a response resource object must contain an id",
            Self::MissingResponseIdentifierId => {
                "a response relationship identifier must contain an id"
            }
            Self::DuplicateResourceIdentifier => {
                "a JSON:API document must not repeat a resource identifier"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for DocumentValidationError {}

fn validate_response_resource_id(resource: &ResourceObject) -> Result<(), DocumentValidationError> {
    if resource.id.is_none() {
        return Err(DocumentValidationError::MissingResourceId);
    }
    Ok(())
}

fn validate_response_relationship_identifiers(
    resource: &ResourceObject,
) -> Result<(), DocumentValidationError> {
    if let Some(relationships) = &resource.relationships {
        for (name, relationship) in relationships {
            if is_at_member(name) {
                continue;
            }
            let identifiers = match relationship.data.as_ref() {
                Some(RelationshipData::One(identifier)) => std::slice::from_ref(identifier),
                Some(RelationshipData::Many(identifiers)) => identifiers,
                Some(RelationshipData::Null) | None => continue,
            };
            if identifiers.iter().any(|identifier| identifier.id.is_none()) {
                return Err(DocumentValidationError::MissingResponseIdentifierId);
            }
        }
    }
    Ok(())
}

fn track_resource_identity(
    resource: &ResourceObject,
    identities: &mut HashSet<(String, String, String)>,
) -> Result<(), DocumentValidationError> {
    for (kind, value) in [("id", &resource.id), ("lid", &resource.lid)] {
        if let Some(value) = value {
            let identity = (resource.type_name.clone(), kind.to_owned(), value.clone());
            if !identities.insert(identity) {
                return Err(DocumentValidationError::DuplicateResourceIdentifier);
            }
        }
    }
    Ok(())
}

fn resource_identity(
    resource: &ResourceObject,
) -> Result<(String, String, String), DocumentValidationError> {
    match (&resource.id, &resource.lid) {
        (Some(id), None) => Ok((resource.type_name.clone(), "id".to_owned(), id.clone())),
        (None, Some(lid)) => Ok((resource.type_name.clone(), "lid".to_owned(), lid.clone())),
        (None, None) => Err(DocumentValidationError::MissingIdentifier),
        (Some(_), Some(_)) => Err(DocumentValidationError::BothIdentifiers),
    }
}

fn identifier_identity(
    identifier: &ResourceIdentifier,
) -> Result<(String, String, String), DocumentValidationError> {
    match (&identifier.id, &identifier.lid) {
        (Some(id), None) => Ok((identifier.type_name.clone(), "id".to_owned(), id.clone())),
        (None, Some(lid)) => Ok((identifier.type_name.clone(), "lid".to_owned(), lid.clone())),
        (None, None) => Err(DocumentValidationError::MissingIdentifier),
        (Some(_), Some(_)) => Err(DocumentValidationError::BothIdentifiers),
    }
}

fn validate_included_reachability(
    primary: Option<&PrimaryData>,
    included: &[ResourceObject],
) -> Result<(), DocumentValidationError> {
    let included_by_identity = included
        .iter()
        .map(|resource| Ok((resource_identity(resource)?, resource)))
        .collect::<Result<HashMap<_, _>, DocumentValidationError>>()?;
    let mut pending = match primary {
        Some(PrimaryData::One(resource)) => vec![resource],
        Some(PrimaryData::Many(resources)) => resources.iter().collect(),
        Some(PrimaryData::Null) | None => Vec::new(),
    };
    let mut visited = HashSet::new();
    let mut reachable_included = HashSet::new();

    while let Some(resource) = pending.pop() {
        let identity = resource_identity(resource)?;
        if !visited.insert(identity.clone()) {
            continue;
        }
        if included_by_identity.contains_key(&identity) {
            reachable_included.insert(identity);
        }
        if let Some(relationships) = &resource.relationships {
            for (name, relationship) in relationships {
                if is_at_member(name) {
                    continue;
                }
                let identifiers = match relationship.data.as_ref() {
                    Some(RelationshipData::One(identifier)) => std::slice::from_ref(identifier),
                    Some(RelationshipData::Many(identifiers)) => identifiers,
                    Some(RelationshipData::Null) | None => continue,
                };
                for identifier in identifiers {
                    if let Some(target) =
                        included_by_identity.get(&identifier_identity(identifier)?)
                    {
                        pending.push(*target);
                    }
                }
            }
        }
    }

    if reachable_included.len() != included.len() {
        return Err(DocumentValidationError::UnreachableIncludedResource);
    }
    Ok(())
}

fn validate_relationship_local_ids(
    primary: Option<&PrimaryData>,
    included: &[ResourceObject],
    resource_identities: &HashSet<(String, String, String)>,
) -> Result<(), DocumentValidationError> {
    let mut resources = match primary {
        Some(PrimaryData::One(resource)) => vec![resource],
        Some(PrimaryData::Many(resources)) => resources.iter().collect(),
        Some(PrimaryData::Null) | None => Vec::new(),
    };
    resources.extend(included);

    for resource in resources {
        let Some(relationships) = &resource.relationships else {
            continue;
        };
        for (name, relationship) in relationships {
            if is_at_member(name) {
                continue;
            }
            let identifiers = match relationship.data.as_ref() {
                Some(RelationshipData::One(identifier)) => std::slice::from_ref(identifier),
                Some(RelationshipData::Many(identifiers)) => identifiers,
                Some(RelationshipData::Null) | None => continue,
            };
            for identifier in identifiers {
                if identifier.lid.is_some()
                    && !resource_identities.contains(&identifier_identity(identifier)?)
                {
                    return Err(DocumentValidationError::UnresolvedLocalIdentifier);
                }
            }
        }
    }
    Ok(())
}

fn validate_type(type_name: &str) -> Result<(), DocumentValidationError> {
    if type_name.is_empty() {
        return Err(DocumentValidationError::EmptyType);
    }
    validate_member_name(type_name)?;
    Ok(())
}

fn validate_member_name(name: &str) -> Result<(), DocumentValidationError> {
    if !is_valid_member_name(name) {
        return Err(DocumentValidationError::InvalidMemberName);
    }
    Ok(())
}

pub(crate) fn is_valid_member_name(name: &str) -> bool {
    let name = name.strip_prefix('@').unwrap_or(name);
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let mut last = first;
    for character in characters {
        if !is_globally_allowed_member_character(character) && !matches!(character, '-' | '_' | ' ')
        {
            return false;
        }
        last = character;
    }
    if !is_globally_allowed_member_character(first) || !is_globally_allowed_member_character(last) {
        return false;
    }
    true
}

pub(crate) fn is_at_member(name: &str) -> bool {
    name.starts_with('@')
}

pub(crate) fn validate_links(
    links: Option<&Map<String, Value>>,
) -> Result<(), DocumentValidationError> {
    let Some(links) = links else {
        return Ok(());
    };
    for (relation, link) in links {
        if is_at_member(relation) {
            validate_member_name(relation)?;
            continue;
        }
        if !is_valid_link_relation_type(relation) || !is_valid_link(link) {
            return Err(DocumentValidationError::InvalidLinkObject);
        }
    }
    Ok(())
}

fn is_valid_link(link: &Value) -> bool {
    match link {
        Value::Null => true,
        Value::String(href) => is_valid_uri_reference(href),
        Value::Object(link_object) => {
            let has_href_or_meta =
                link_object.contains_key("href") || link_object.contains_key("meta");
            let valid_href = link_object
                .get("href")
                .is_none_or(|value| value.as_str().is_some_and(is_valid_uri_reference));
            if !has_href_or_meta || !valid_href {
                return false;
            }
            link_object
                .get("rel")
                .is_none_or(|value| value.as_str().is_some_and(is_valid_link_relation_type))
                && link_object.get("title").is_none_or(Value::is_string)
                && link_object.get("type").is_none_or(Value::is_string)
                && link_object.get("hreflang").is_none_or(|value| {
                    value.as_str().is_some_and(is_valid_language_tag)
                        || value.as_array().is_some_and(|values| {
                            !values.is_empty()
                                && values
                                    .iter()
                                    .all(|tag| tag.as_str().is_some_and(is_valid_language_tag))
                        })
                })
                && link_object.get("meta").is_none_or(Value::is_object)
                && link_object.get("describedby").is_none_or(is_valid_link)
        }
        _ => false,
    }
}

pub(crate) fn is_valid_uri_reference(value: &str) -> bool {
    URIReference::try_from(value).is_ok()
}

pub(crate) fn is_valid_absolute_uri(value: &str) -> bool {
    URIReference::try_from(value)
        .ok()
        .is_some_and(|reference| reference.scheme().is_some())
}

fn is_valid_language_tag(value: &str) -> bool {
    LanguageTag::parse(value).is_ok()
}

fn is_valid_json_pointer(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let Some(tokens) = value.strip_prefix('/') else {
        return false;
    };
    let mut bytes = tokens.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

fn is_valid_http_status(value: &str) -> bool {
    value.len() == 3
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value
            .parse::<u16>()
            .is_ok_and(|status| (100..=599).contains(&status))
}

fn is_valid_link_relation_type(value: &str) -> bool {
    if is_registered_link_relation_type(value) {
        return true;
    }
    is_valid_absolute_uri(value)
}

fn is_registered_link_relation_type(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_globally_allowed_member_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character >= '\u{0080}'
}

fn deserialize_primary_data<'de, D>(deserializer: D) -> Result<Option<PrimaryData>, D::Error>
where
    D: Deserializer<'de>,
{
    PrimaryData::deserialize(deserializer).map(Some)
}

fn deserialize_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_relationship_data<'de, D>(
    deserializer: D,
) -> Result<Option<RelationshipData>, D::Error>
where
    D: Deserializer<'de>,
{
    RelationshipData::deserialize(deserializer).map(Some)
}

fn deserialize_object_map_ignoring_at_members<'de, D>(
    deserializer: D,
) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = ObjectOnly::<Map<String, Value>>::deserialize(deserializer)?.into_inner();
    remove_at_members(values)
}

pub(crate) fn deserialize_links<'de, D>(
    deserializer: D,
) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = ObjectOnly::<Map<String, Value>>::deserialize(deserializer)?.into_inner();
    let Some(mut links) = remove_at_members(values)? else {
        return Ok(None);
    };
    for link in links.values_mut() {
        remove_at_members_from_link(link)?;
    }
    Ok(Some(links))
}

pub(crate) fn deserialize_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_object_map_ignoring_at_members(deserializer)
}

fn remove_at_members<E>(values: Map<String, Value>) -> Result<Option<Map<String, Value>>, E>
where
    E: serde::de::Error,
{
    let had_members = !values.is_empty();
    let mut members = Map::new();
    for (name, value) in values {
        if is_at_member(&name) {
            validate_member_name(&name).map_err(E::custom)?;
        } else {
            members.insert(name, value);
        }
    }
    if had_members && members.is_empty() {
        Ok(None)
    } else {
        Ok(Some(members))
    }
}

fn remove_at_members_from_link<E>(link: &mut Value) -> Result<(), E>
where
    E: serde::de::Error,
{
    let Value::Object(link_object) = link else {
        return Ok(());
    };
    let mut link_object = std::mem::take(link_object);
    let metadata = link_object.remove("meta");
    let describedby = link_object.remove("describedby");
    let mut link_object = remove_at_members(link_object)?.unwrap_or_default();
    if let Some(metadata) = metadata {
        let metadata = match metadata {
            Value::Object(metadata) => remove_at_members(metadata)?.map(Value::Object),
            value => Some(value),
        };
        if let Some(metadata) = metadata {
            link_object.insert("meta".to_owned(), metadata);
        }
    }
    if let Some(mut describedby) = describedby {
        remove_at_members_from_link(&mut describedby)?;
        link_object.insert("describedby".to_owned(), describedby);
    }
    *link = Value::Object(link_object);
    Ok(())
}

pub(crate) fn deserialize_relationships<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, Relationship>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Map::<String, Value>::deserialize(deserializer)?;
    let had_members = !values.is_empty();
    let mut relationships = BTreeMap::new();
    for (name, value) in values {
        if is_at_member(&name) {
            validate_member_name(&name).map_err(D::Error::custom)?;
            continue;
        }
        let relationship = serde_json::from_value(value).map_err(D::Error::custom)?;
        relationships.insert(name, relationship);
    }
    if had_members && relationships.is_empty() {
        Ok(None)
    } else {
        Ok(Some(relationships))
    }
}

pub(crate) fn deserialize_attributes<'de, D>(
    deserializer: D,
) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Map::<String, Value>::deserialize(deserializer)?;
    let had_members = !values.is_empty();
    for name in values.keys().filter(|name| is_at_member(name)) {
        validate_member_name(name).map_err(D::Error::custom)?;
    }
    let attributes = values
        .into_iter()
        .filter(|(name, _)| !is_at_member(name))
        .collect::<Map<_, _>>();
    if had_members && attributes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(attributes))
    }
}
