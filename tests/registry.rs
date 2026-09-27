#![allow(missing_docs)]

use seamark::registry::{RegistryError, ResourceDefinition, ResourceRegistry};

fn ports() -> ResourceDefinition {
    ResourceDefinition::new("ports", "id")
        .attribute("name", "title", true, true)
        .attribute("capacity", "berth_count", false, false)
        .relationship("owner", "owner", "people")
}

fn people() -> ResourceDefinition {
    ResourceDefinition::new("people", "person_id").attribute("name", "full_name", true, false)
}

#[test]
fn registers_resources_and_resolves_explicit_mappings() {
    let registry = ResourceRegistry::new([ports(), people()]).unwrap();

    let port = registry.resource("ports").unwrap();
    assert_eq!(port.identifier_field(), "id");
    let name = registry.filterable_attribute("ports", "name").unwrap();
    assert_eq!(name.model_field(), "title");
    assert!(name.is_filterable());
    assert!(name.is_sortable());

    let owner = registry.relationship("ports", "owner").unwrap();
    assert_eq!(owner.model_field(), "owner");
    assert_eq!(owner.target_type(), "people");
    assert_eq!(
        registry.attribute("people", "name").unwrap().model_field(),
        "full_name"
    );
    assert_eq!(
        registry.attribute("ports", "name").unwrap().model_field(),
        "title"
    );
}

#[test]
fn rejects_unknown_resources_and_fields() {
    let registry = ResourceRegistry::new([ports(), people()]).unwrap();

    assert_eq!(
        registry.resource("ships"),
        Err(RegistryError::UnknownResourceType("ships".to_owned()))
    );
    assert_eq!(
        registry.attribute("ports", "secret"),
        Err(RegistryError::UnknownField {
            resource_type: "ports".to_owned(),
            field_name: "secret".to_owned()
        })
    );
}

#[test]
fn enforces_explicit_filter_and_sort_opt_in() {
    let registry = ResourceRegistry::new([ports(), people()]).unwrap();

    assert_eq!(
        registry.filterable_attribute("ports", "capacity"),
        Err(RegistryError::AttributeNotFilterable {
            resource_type: "ports".to_owned(),
            field_name: "capacity".to_owned()
        })
    );
    assert_eq!(
        registry.sortable_attribute("people", "name"),
        Err(RegistryError::AttributeNotSortable {
            resource_type: "people".to_owned(),
            field_name: "name".to_owned()
        })
    );
}

#[test]
fn rejects_duplicate_resource_types_and_public_fields() {
    assert_eq!(
        ResourceRegistry::new([ports(), ports()]),
        Err(RegistryError::DuplicateResourceType("ports".to_owned()))
    );

    let duplicate_field = ResourceDefinition::new("ports", "id")
        .attribute("name", "title", false, false)
        .relationship("name", "owner", "people");
    assert_eq!(
        ResourceRegistry::new([duplicate_field, people()]),
        Err(RegistryError::DuplicateFieldName {
            resource_type: "ports".to_owned(),
            field_name: "name".to_owned()
        })
    );

    let duplicate_attributes = ResourceDefinition::new("ports", "id")
        .attribute("name", "title", false, false)
        .attribute("name", "label", false, false);
    assert_eq!(
        ResourceRegistry::new([duplicate_attributes]),
        Err(RegistryError::DuplicateFieldName {
            resource_type: "ports".to_owned(),
            field_name: "name".to_owned()
        })
    );

    let duplicate_relationships = ResourceDefinition::new("ports", "id")
        .relationship("owner", "owner", "people")
        .relationship("owner", "alternate_owner", "people");
    assert_eq!(
        ResourceRegistry::new([duplicate_relationships, people()]),
        Err(RegistryError::DuplicateFieldName {
            resource_type: "ports".to_owned(),
            field_name: "owner".to_owned()
        })
    );
}

#[test]
fn rejects_colliding_internal_field_mappings() {
    let collisions = [
        (
            ResourceDefinition::new("ports", "id").attribute("key", "id", false, false),
            "id",
        ),
        (
            ResourceDefinition::new("ports", "id")
                .attribute("name", "title", false, false)
                .attribute("label", "title", false, false),
            "title",
        ),
        (
            ResourceDefinition::new("ports", "id")
                .attribute("ownerName", "owner", false, false)
                .relationship("owner", "owner", "people"),
            "owner",
        ),
        (
            ResourceDefinition::new("ports", "id")
                .relationship("owner", "owner", "people")
                .relationship("backupOwner", "owner", "people"),
            "owner",
        ),
    ];

    for (definition, model_field) in collisions {
        assert_eq!(
            ResourceRegistry::new([definition, people()]).unwrap_err(),
            RegistryError::DuplicateModelField {
                resource_type: "ports".to_owned(),
                model_field: model_field.to_owned()
            }
        );
    }
}

#[test]
fn rejects_invalid_fields_identifiers_and_relationship_targets() {
    assert_eq!(
        ResourceRegistry::new([ResourceDefinition::new("", "id")]),
        Err(RegistryError::EmptyResourceType)
    );
    assert_eq!(
        ResourceRegistry::new([ResourceDefinition::new("ports", "")]),
        Err(RegistryError::EmptyIdentifierField {
            resource_type: "ports".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").attribute("type", "kind", false, false)
        ]),
        Err(RegistryError::ReservedFieldName {
            resource_type: "ports".to_owned(),
            field_name: "type".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").attribute("id", "key", false, false)
        ]),
        Err(RegistryError::ReservedFieldName {
            resource_type: "ports".to_owned(),
            field_name: "id".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").relationship("id", "port_id", "people"),
            people()
        ]),
        Err(RegistryError::ReservedFieldName {
            resource_type: "ports".to_owned(),
            field_name: "id".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").attribute("", "field", false, false)
        ]),
        Err(RegistryError::EmptyFieldName {
            resource_type: "ports".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").attribute("name", "", false, false)
        ]),
        Err(RegistryError::EmptyModelField {
            resource_type: "ports".to_owned(),
            field_name: "name".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").relationship("", "owner", "people"),
            people()
        ]),
        Err(RegistryError::EmptyFieldName {
            resource_type: "ports".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").relationship("owner", "", "people"),
            people()
        ]),
        Err(RegistryError::EmptyModelField {
            resource_type: "ports".to_owned(),
            field_name: "owner".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").relationship("owner", "owner", "people")
        ]),
        Err(RegistryError::UnknownRelationshipTarget {
            resource_type: "ports".to_owned(),
            relationship: "owner".to_owned(),
            target_type: "people".to_owned()
        })
    );
    assert_eq!(
        ResourceRegistry::new([
            ResourceDefinition::new("ports", "id").relationship("owner", "owner", "")
        ]),
        Err(RegistryError::EmptyRelationshipTarget {
            resource_type: "ports".to_owned(),
            relationship: "owner".to_owned()
        })
    );
}

#[test]
fn accepts_self_referential_relationship_targets() {
    let categories = ResourceDefinition::new("categories", "category_id").relationship(
        "parent",
        "parent",
        "categories",
    );
    let registry = ResourceRegistry::new([categories]).unwrap();

    assert_eq!(
        registry
            .relationship("categories", "parent")
            .unwrap()
            .target_type(),
        "categories"
    );
}

#[test]
fn failed_batch_registration_returns_no_registry() {
    let invalid =
        ResourceDefinition::new("ports", "id").relationship("owner", "owner", "missing-people");

    assert_eq!(
        ResourceRegistry::new([invalid, people()]),
        Err(RegistryError::UnknownRelationshipTarget {
            resource_type: "ports".to_owned(),
            relationship: "owner".to_owned(),
            target_type: "missing-people".to_owned()
        })
    );
}
