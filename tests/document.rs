#![allow(missing_docs)]

use seamark::document::{
    DocumentValidationError, ErrorObject, ErrorSource, JsonApiDocument, PrimaryData, Relationship,
    RelationshipData, ResourceIdentifier, ResourceObject,
};
use serde_json::json;
use std::collections::BTreeMap;

fn resource(type_name: &str, id: &str) -> ResourceObject {
    ResourceObject {
        type_name: type_name.to_owned(),
        id: Some(id.to_owned()),
        lid: None,
        attributes: None,
        relationships: None,
        links: None,
        meta: None,
    }
}

fn local_resource(type_name: &str, lid: &str) -> ResourceObject {
    ResourceObject {
        type_name: type_name.to_owned(),
        id: None,
        lid: Some(lid.to_owned()),
        attributes: None,
        relationships: None,
        links: None,
        meta: None,
    }
}

fn relationship_to(type_name: &str, id: &str) -> Relationship {
    Relationship {
        data: Some(RelationshipData::One(ResourceIdentifier {
            type_name: type_name.to_owned(),
            id: Some(id.to_owned()),
            ..ResourceIdentifier::default()
        })),
        ..Relationship::default()
    }
}

#[test]
fn serializes_a_resource_document_with_json_api_member_names() {
    let mut attributes = serde_json::Map::new();
    attributes.insert("name".to_owned(), json!("Harbor"));
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(ResourceObject {
            attributes: Some(attributes),
            ..resource("ports", "1")
        })),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        serde_json::to_value(&document).unwrap(),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "attributes": {"name": "Harbor"}
            }
        })
    );
    document.validate().unwrap();
}

#[test]
fn round_trips_null_and_collection_primary_data() {
    for data in [
        PrimaryData::Null,
        PrimaryData::Many(vec![]),
        PrimaryData::Many(vec![resource("ports", "1"), resource("ports", "2")]),
    ] {
        let document = JsonApiDocument {
            data: Some(data),
            ..JsonApiDocument::default()
        };
        let encoded = serde_json::to_string(&document).unwrap();
        let decoded: JsonApiDocument = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, document);
        decoded.validate().unwrap();
    }
}

#[test]
fn accepts_local_id_resource_objects_but_requires_response_ids() {
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(ResourceObject {
            type_name: "ports".to_owned(),
            id: None,
            lid: Some("new-port".to_owned()),
            attributes: None,
            relationships: None,
            links: None,
            meta: None,
        })),
        ..JsonApiDocument::default()
    };
    document.validate().unwrap();
    assert_eq!(
        document.validate_response(),
        Err(DocumentValidationError::MissingResourceId)
    );
    assert_eq!(
        serde_json::to_value(&document).unwrap(),
        json!({"data": {"type": "ports", "lid": "new-port"}})
    );
}

#[test]
fn response_relationship_identifiers_require_persistent_ids() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people", "lid": "new-owner"}}
            }
        }
    }))
    .unwrap();
    document.validate().unwrap();
    assert_eq!(
        document.validate_response(),
        Err(DocumentValidationError::MissingResponseIdentifierId)
    );

    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people", "id": "2"}}
            }
        }
    }))
    .unwrap();
    valid.validate_response().unwrap();
}

#[test]
fn distinguishes_explicit_null_from_omitted_document_and_relationship_data() {
    let explicit_null: JsonApiDocument = serde_json::from_value(json!({"data": null})).unwrap();
    assert_eq!(explicit_null.data, Some(PrimaryData::Null));
    explicit_null.validate().unwrap();

    let omitted: JsonApiDocument = serde_json::from_value(json!({"meta": {}})).unwrap();
    assert_eq!(omitted.data, None);
    omitted.validate().unwrap();

    let relationship: Relationship = serde_json::from_value(json!({"data": null})).unwrap();
    assert_eq!(relationship.data, Some(RelationshipData::Null));
}

#[test]
fn rejects_null_for_optional_members_that_require_objects_or_arrays() {
    for value in [
        json!({"errors": null, "meta": {}}),
        json!({"data": null, "included": null}),
        json!({"meta": null}),
        json!({"jsonapi": null, "meta": {}}),
        json!({"links": null, "meta": {}}),
        json!({"data": {"type": "ports", "id": "1", "attributes": null}}),
        json!({"data": {"type": "ports", "id": null, "lid": "new-port"}}),
        json!({"data": {"type": "ports", "id": "1", "relationships": null}}),
        json!({"errors": [{"status": null}]}),
    ] {
        assert!(serde_json::from_value::<JsonApiDocument>(value).is_err());
    }
}

#[test]
fn preserves_null_attribute_values_and_distinguishes_omitted_attributes() {
    let with_null_attribute: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "attributes": {"description": null}
        }
    }))
    .unwrap();
    assert_eq!(
        with_null_attribute.data,
        Some(PrimaryData::One(ResourceObject {
            attributes: Some(serde_json::Map::from_iter([(
                "description".to_owned(),
                serde_json::Value::Null
            )])),
            ..resource("ports", "1")
        }))
    );

    let omitted_attributes: JsonApiDocument = serde_json::from_value(json!({
        "data": {"type": "ports", "id": "1"}
    }))
    .unwrap();
    let Some(PrimaryData::One(omitted_resource)) = omitted_attributes.data else {
        panic!("expected one resource");
    };
    assert_eq!(omitted_resource.attributes, None);
}

#[test]
fn serializes_error_sources_and_status_as_strings() {
    let document = JsonApiDocument {
        errors: Some(vec![ErrorObject {
            status: Some("422".to_owned()),
            title: Some("Invalid attribute".to_owned()),
            source: Some(ErrorSource {
                pointer: Some("/data/attributes/name".to_owned()),
                ..ErrorSource::default()
            }),
            ..ErrorObject::default()
        }]),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        serde_json::to_value(document).unwrap(),
        json!({
            "errors": [{
                "status": "422",
                "title": "Invalid attribute",
                "source": {"pointer": "/data/attributes/name"}
            }]
        })
    );
}

#[test]
fn accepts_metadata_only_documents() {
    let mut meta = serde_json::Map::new();
    meta.insert("total".to_owned(), json!(0));
    JsonApiDocument {
        meta: Some(meta),
        ..JsonApiDocument::default()
    }
    .validate()
    .unwrap();
}

#[test]
fn rejects_documents_without_data_errors_or_meta() {
    assert_eq!(
        JsonApiDocument::default().validate(),
        Err(DocumentValidationError::MissingContent)
    );
}

#[test]
fn rejects_documents_containing_both_data_and_errors() {
    let document = JsonApiDocument {
        data: Some(PrimaryData::Null),
        errors: Some(vec![ErrorObject::default()]),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::DataAndErrors)
    );
}

#[test]
fn rejects_an_empty_errors_array() {
    let document = JsonApiDocument {
        errors: Some(vec![]),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::EmptyErrors)
    );
}

#[test]
fn error_objects_must_contain_at_least_one_defined_member() {
    let empty = JsonApiDocument {
        errors: Some(vec![ErrorObject::default()]),
        ..JsonApiDocument::default()
    };
    assert_eq!(
        empty.validate(),
        Err(DocumentValidationError::EmptyErrorObject)
    );

    for error in [
        ErrorObject {
            id: Some("occurrence-1".to_owned()),
            ..ErrorObject::default()
        },
        ErrorObject {
            meta: Some(serde_json::Map::new()),
            ..ErrorObject::default()
        },
    ] {
        JsonApiDocument {
            errors: Some(vec![error]),
            ..JsonApiDocument::default()
        }
        .validate()
        .unwrap();
    }
}

#[test]
fn validates_link_values_and_link_object_shapes_in_all_document_contexts() {
    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "links": {
                "self": "/ports/1",
                "describedby": {
                    "href": "/schemas/ports",
                    "hreflang": ["en", "fr"],
                    "meta": {"version": 1}
                }
            },
            "relationships": {
                "owner": {
                    "links": {
                        "related": null,
                        "self": {"href": "/ports/1/relationships/owner", "rel": "self"}
                    }
                }
            }
        },
        "links": {"self": {"href": "/ports/1", "type": "application/vnd.api+json"}}
    }))
    .unwrap();
    valid.validate().unwrap();

    for document in [
        json!({"data": null, "links": {"self": 42}}),
        json!({"data": {"type": "ports", "id": "1", "links": {"self": {}}}}),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"links": {"related": {"href": 1}}}}
            }
        }),
        json!({"errors": [{"links": {"about": {"href": "/errors/1", "hreflang": [1]}}}]}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(document).unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidLinkObject)
        );
    }
}

#[test]
fn rejects_included_resources_without_primary_data() {
    let document = JsonApiDocument {
        included: Some(vec![resource("ports", "1")]),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::IncludedWithoutData)
    );
}

#[test]
fn validates_included_resource_reachability_through_relationship_linkage() {
    let mut primary = resource("ports", "1");
    primary.relationships = Some(BTreeMap::from([(
        "owner".to_owned(),
        relationship_to("people", "2"),
    )]));
    let mut owner = resource("people", "2");
    owner.relationships = Some(BTreeMap::from([(
        "team".to_owned(),
        relationship_to("teams", "3"),
    )]));
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(primary)),
        included: Some(vec![owner, resource("teams", "3")]),
        ..JsonApiDocument::default()
    };
    document.validate_response().unwrap();

    let unreachable = JsonApiDocument {
        data: Some(PrimaryData::One(resource("ports", "1"))),
        included: Some(vec![resource("people", "2")]),
        ..JsonApiDocument::default()
    };
    assert_eq!(
        unreachable.validate(),
        Err(DocumentValidationError::UnreachableIncludedResource)
    );
}

#[test]
fn resource_objects_and_identifiers_must_not_contain_both_id_and_lid() {
    let resource_with_both: JsonApiDocument = serde_json::from_value(json!({
        "data": {"type": "ports", "id": "1", "lid": "local"}
    }))
    .unwrap();
    assert_eq!(
        resource_with_both.validate(),
        Err(DocumentValidationError::BothIdentifiers)
    );

    let identifier_with_both: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people", "id": "2", "lid": "local"}}
            }
        }
    }))
    .unwrap();
    assert_eq!(
        identifier_with_both.validate(),
        Err(DocumentValidationError::BothIdentifiers)
    );
}

#[test]
fn resource_fields_must_not_conflict_with_type_id_or_each_other() {
    for data in [
        json!({"type": "ports", "id": "1", "attributes": {"type": "nested"}}),
        json!({"type": "ports", "id": "1", "attributes": {"id": "nested"}}),
        json!({"type": "ports", "id": "1", "relationships": {"type": {"data": null}}}),
        json!({"type": "ports", "id": "1", "relationships": {"id": {"data": null}}}),
        json!({
            "type": "ports",
            "id": "1",
            "attributes": {"owner": "not a relationship"},
            "relationships": {"owner": {"data": null}}
        }),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({"data": data})).unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::ConflictingFieldName)
        );
    }
}

#[test]
fn validates_jsonapi_member_name_rules_for_types_and_fields() {
    for data in [
        json!({"type": "bad.type", "id": "1"}),
        json!({"type": "ports", "id": "1", "attributes": {"bad/name": "value"}}),
        json!({"type": "ports", "id": "1", "relationships": {"trailing-": {"data": null}}}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({"data": data})).unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidMemberName)
        );
    }

    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "shore craft",
            "id": "1",
            "attributes": {"well-known": "value", "naïve": true},
            "relationships": {"@related": {"data": null}}
        }
    }))
    .unwrap();
    valid.validate().unwrap();
}

#[test]
fn relationship_objects_require_linkage_links_or_metadata() {
    let empty: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {"owner": {}}
        }
    }))
    .unwrap();
    assert_eq!(
        empty.validate(),
        Err(DocumentValidationError::EmptyRelationship)
    );

    for relationship in [
        json!({"links": {"related": "/ports/1/owner"}}),
        json!({"meta": {}}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": relationship}
            }
        }))
        .unwrap();
        document.validate().unwrap();
    }
}

#[test]
fn validates_relationship_identifier_objects_and_local_ids() {
    let mut relationships = BTreeMap::new();
    relationships.insert(
        "owner".to_owned(),
        Relationship {
            data: Some(RelationshipData::One(ResourceIdentifier {
                type_name: "people".to_owned(),
                id: None,
                lid: Some("new-owner".to_owned()),
                meta: None,
            })),
            ..Relationship::default()
        },
    );
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(ResourceObject {
            relationships: Some(relationships),
            ..resource("ports", "1")
        })),
        ..JsonApiDocument::default()
    };

    document.validate().unwrap();
}

#[test]
fn relationship_linkage_round_trips_null_single_and_multiple_identifiers() {
    let identifiers = vec![
        ResourceIdentifier {
            type_name: "people".to_owned(),
            id: Some("1".to_owned()),
            ..ResourceIdentifier::default()
        },
        ResourceIdentifier {
            type_name: "people".to_owned(),
            lid: Some("new-person".to_owned()),
            ..ResourceIdentifier::default()
        },
    ];
    for data in [
        RelationshipData::Null,
        RelationshipData::One(identifiers[0].clone()),
        RelationshipData::One(identifiers[1].clone()),
        RelationshipData::Many(identifiers),
    ] {
        let relationship = Relationship {
            data: Some(data),
            ..Relationship::default()
        };
        let encoded = serde_json::to_string(&relationship).unwrap();
        let decoded: Relationship = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, relationship);
    }
}

#[test]
fn rejects_duplicate_document_members() {
    let result = serde_json::from_str::<JsonApiDocument>(r#"{"meta":{},"meta":{}}"#);
    assert!(result.is_err());
}

#[test]
fn rejects_duplicate_resource_identifiers_in_a_collection() {
    let duplicate_id = JsonApiDocument {
        data: Some(PrimaryData::Many(vec![
            resource("ports", "1"),
            resource("ports", "1"),
        ])),
        ..JsonApiDocument::default()
    };
    assert_eq!(
        duplicate_id.validate(),
        Err(DocumentValidationError::DuplicateResourceIdentifier)
    );

    let duplicate_lid = JsonApiDocument {
        data: Some(PrimaryData::Many(vec![
            local_resource("ports", "new-resource"),
            local_resource("ports", "new-resource"),
        ])),
        ..JsonApiDocument::default()
    };
    assert_eq!(
        duplicate_lid.validate(),
        Err(DocumentValidationError::DuplicateResourceIdentifier)
    );

    let duplicate_across_primary_and_included = JsonApiDocument {
        data: Some(PrimaryData::One(resource("ports", "1"))),
        included: Some(vec![resource("ports", "1")]),
        ..JsonApiDocument::default()
    };
    assert_eq!(
        duplicate_across_primary_and_included.validate(),
        Err(DocumentValidationError::DuplicateResourceIdentifier)
    );

    let same_id_on_different_resource_types = JsonApiDocument {
        data: Some(PrimaryData::Many(vec![
            resource("ports", "1"),
            resource("people", "1"),
        ])),
        ..JsonApiDocument::default()
    };
    same_id_on_different_resource_types.validate().unwrap();

    let same_lid_on_different_resource_types = JsonApiDocument {
        data: Some(PrimaryData::Many(vec![
            local_resource("ports", "new-resource"),
            local_resource("people", "new-resource"),
        ])),
        ..JsonApiDocument::default()
    };
    same_lid_on_different_resource_types.validate().unwrap();
}

#[test]
fn rejects_resources_with_an_empty_type() {
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(resource("", "1"))),
        ..JsonApiDocument::default()
    };

    assert_eq!(document.validate(), Err(DocumentValidationError::EmptyType));
}

#[test]
fn rejects_resource_objects_without_id_or_lid() {
    let document = JsonApiDocument {
        data: Some(PrimaryData::One(ResourceObject {
            type_name: "ports".to_owned(),
            id: None,
            lid: None,
            attributes: None,
            relationships: None,
            links: None,
            meta: None,
        })),
        ..JsonApiDocument::default()
    };

    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::MissingIdentifier)
    );
}

#[test]
fn rejects_identifiers_without_type_or_identity() {
    for (type_name, expected) in [
        ("", DocumentValidationError::EmptyType),
        ("people", DocumentValidationError::MissingIdentifier),
    ] {
        let identifier = ResourceIdentifier {
            type_name: type_name.to_owned(),
            ..ResourceIdentifier::default()
        };
        let mut relationships = BTreeMap::new();
        relationships.insert(
            "owner".to_owned(),
            Relationship {
                data: Some(RelationshipData::One(identifier)),
                ..Relationship::default()
            },
        );
        let document = JsonApiDocument {
            data: Some(PrimaryData::One(ResourceObject {
                relationships: Some(relationships),
                ..resource("ports", "1")
            })),
            ..JsonApiDocument::default()
        };

        assert_eq!(document.validate(), Err(expected));
    }
}
