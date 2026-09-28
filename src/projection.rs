//! Registry-based public resource projection shared by HTTP and persistence adapters.

use std::collections::{BTreeMap, BTreeSet};

use crate::document::{Relationship, RelationshipData, ResourceObject};
use crate::http::AdapterResource;
use crate::query::{IncludeNode, PlannedField, ReadPlan};
use crate::registry::{AttributePermission, RelationshipPermission, ResourceDefinition};

/// A failure while projecting persistence fields into the public resource shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    /// The included resource type is not registered.
    UnknownResourceType,
    /// A mapped relationship contains linkage for a different resource type.
    RelationshipTargetMismatch,
}

/// Removes undeclared or unexposed values from an adapter record and applies
/// an optional sparse fieldset. Both HTTP responses and included resources
/// should pass through this function before serialization.
#[must_use]
pub fn project_adapter_record(
    definition: &ResourceDefinition,
    resource: AdapterResource,
    fieldset: Option<&[PlannedField]>,
) -> AdapterResource {
    project_adapter_record_with_includes(definition, resource, fieldset, &BTreeSet::new())
}

pub(crate) fn project_adapter_record_with_includes(
    definition: &ResourceDefinition,
    mut resource: AdapterResource,
    fieldset: Option<&[PlannedField]>,
    include_relationships: &BTreeSet<String>,
) -> AdapterResource {
    resource.attributes.retain(|model_field, _| {
        definition.attributes().iter().any(|attribute| {
            attribute.model_field() == model_field && attribute.allows(AttributePermission::Read)
        })
    });
    resource.relationships.retain(|model_field, _| {
        definition.relationships().iter().any(|relationship| {
            relationship.model_field() == model_field
                && (relationship.allows(RelationshipPermission::Read)
                    || (relationship.allows(RelationshipPermission::Include)
                        && include_relationships.contains(relationship.public_name())))
        })
    });
    if let Some(fields) = fieldset {
        let attributes = fields
            .iter()
            .filter_map(|field| match field {
                PlannedField::Attribute { model_field, .. } => Some(model_field.as_str()),
                PlannedField::Relationship { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        let relationships = fields
            .iter()
            .filter_map(|field| match field {
                PlannedField::Relationship { model_field, .. } => Some(model_field.as_str()),
                PlannedField::Attribute { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        resource
            .attributes
            .retain(|model_field, _| attributes.contains(model_field.as_str()));
        resource
            .relationships
            .retain(|model_field, _| relationships.contains(model_field.as_str()));
    }
    resource
}

/// Converts a mapped adapter record to a JSON:API resource object using only
/// fields explicitly exposed by its resource definition.
///
/// # Errors
///
/// Returns an error if relationship linkage contradicts the declared target
/// type.
pub fn project_resource(
    definition: &ResourceDefinition,
    record: &AdapterResource,
) -> Result<ResourceObject, ProjectionError> {
    project_resource_with_includes(definition, record, &BTreeSet::new())
}

/// Projects a resource and includes linkage required to make planned included
/// resources reachable even when ordinary relationship reads are disabled.
pub(crate) fn project_resource_with_includes(
    definition: &ResourceDefinition,
    record: &AdapterResource,
    included_relationships: &BTreeSet<String>,
) -> Result<ResourceObject, ProjectionError> {
    let attributes = definition
        .attributes()
        .iter()
        .filter(|mapping| mapping.allows(AttributePermission::Read))
        .filter_map(|mapping| {
            record
                .attributes
                .get(mapping.model_field())
                .map(|value| (mapping.public_name().to_owned(), value.clone()))
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let relationships = definition
        .relationships()
        .iter()
        .filter(|mapping| {
            mapping.allows(RelationshipPermission::Read)
                || (mapping.allows(RelationshipPermission::Include)
                    && included_relationships.contains(mapping.public_name()))
        })
        .filter_map(|mapping| {
            record
                .relationships
                .get(mapping.model_field())
                .map(|relationship| (mapping, relationship))
        })
        .map(|(mapping, relationship)| {
            if !relationship_matches_target(relationship, mapping.target_type()) {
                return Err(ProjectionError::RelationshipTargetMismatch);
            }
            Ok((mapping.public_name().to_owned(), relationship.clone()))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;

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

pub(crate) fn include_relationships_by_type(plan: &ReadPlan) -> BTreeMap<String, BTreeSet<String>> {
    fn collect(
        resource_type: &str,
        includes: &[IncludeNode],
        result: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        for include in includes {
            result
                .entry(resource_type.to_owned())
                .or_default()
                .insert(include.public_name.clone());
            collect(&include.target_type, &include.children, result);
        }
    }
    let mut result = BTreeMap::new();
    collect(&plan.resource_type, &plan.includes, &mut result);
    result
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
