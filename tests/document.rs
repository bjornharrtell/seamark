#![allow(missing_docs)]

use language_tags::LanguageTag;
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
fn document_and_nested_objects_must_be_json_objects() {
    for value in [json!([]), json!("document"), json!(null), json!(7)] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(value.clone()).is_err(),
            "expected non-object document root {value} to be rejected"
        );
    }

    for value in [
        json!({"data": ["ports", "1"]}),
        json!({"data": {"type": "ports", "id": "1", "relationships": {"owner": ["people", "2"]}}}),
        json!({"data": {"type": "ports", "id": "1", "relationships": {"owner": {"data": ["people", "2"]}}}}),
        json!({"errors": [["400"]]}),
        json!({"errors": [{"status": "400", "source": ["/data"]}]}),
        json!({"meta": {}, "jsonapi": ["1.1"]}),
    ] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(value.clone()).is_err(),
            "expected JSON:API object shapes to reject array values: {value}"
        );
    }
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
fn ignores_unrecognized_members_in_documents_and_base_objects() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "unrecognizedResourceMember": true,
            "relationships": {
                "owner": {
                    "data": {
                        "type": "people",
                        "id": "2",
                        "unrecognizedIdentifierMember": true
                    },
                    "unrecognizedRelationshipMember": true
                }
            }
        },
        "jsonapi": {"unrecognizedJsonApiMember": true},
        "unrecognizedDocumentMember": true
    }))
    .unwrap();

    document.validate_response().unwrap();
    assert_eq!(
        serde_json::to_value(document).unwrap(),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {
                    "owner": {"data": {"type": "people", "id": "2"}}
                }
            },
            "jsonapi": {}
        })
    );
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
fn accepts_empty_resource_and_local_identifier_strings() {
    let response: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "",
            "relationships": {
                "owner": {"data": {"type": "people", "id": ""}}
            }
        }
    }))
    .unwrap();
    response.validate_response().unwrap();

    let request: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "lid": "",
            "relationships": {
                "owner": {"data": {"type": "people", "lid": ""}}
            }
        },
        "included": [{"type": "people", "lid": ""}]
    }))
    .unwrap();
    request.validate().unwrap();
}

#[test]
fn resource_object_ids_must_be_strings() {
    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": {"type": "ports", "id": "1"}
    }))
    .unwrap();
    valid.validate().unwrap();

    assert!(
        serde_json::from_value::<JsonApiDocument>(json!({
            "data": {"type": "ports", "id": 1}
        }))
        .is_err()
    );
}

#[test]
fn resource_and_linkage_identity_members_must_be_strings() {
    for value in [
        json!({"data": {"type": "ports", "id": "1"}}),
        json!({"data": {"type": "ports", "lid": "local-port"}}),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"data": {"type": "people", "id": "2"}}}
            }
        }),
        json!({
            "data": {
                "type": "ports",
                "lid": "local-port",
                "relationships": {"owner": {"data": {"type": "people", "lid": "local-owner"}}}
            },
            "included": [{"type": "people", "lid": "local-owner"}]
        }),
    ] {
        let document: JsonApiDocument = serde_json::from_value(value).unwrap();
        document.validate().unwrap();
    }

    for value in [
        json!({"data": {"type": 1, "id": "1"}}),
        json!({"data": {"type": "ports", "id": 1}}),
        json!({"data": {"type": "ports", "lid": 1}}),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"data": {"type": 1, "id": "2"}}}
            }
        }),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"data": {"type": "people", "id": 2}}}
            }
        }),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"data": {"type": "people", "lid": 2}}}
            }
        }),
    ] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(value).is_err(),
            "identity members must be JSON strings"
        );
    }
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
        },
        "included": [{"type": "people", "lid": "new-owner"}]
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

    document.validate().unwrap();
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
fn validates_json_pointer_syntax_in_error_sources() {
    for pointer in ["", "/", "/data/attributes/name", "/a~1b/c~0d", "/~01"] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "errors": [{"source": {"pointer": pointer}}]
        }))
        .unwrap();
        document.validate().unwrap();
    }

    for pointer in ["data/attributes/name", "/a~", "/a~2b"] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "errors": [{"source": {"pointer": pointer}}]
        }))
        .unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidErrorSourcePointer)
        );
    }
}

#[test]
fn validates_error_status_codes_as_http_status_strings() {
    for status in ["100", "200", "422", "599"] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "errors": [{"status": status}]
        }))
        .unwrap();
        document.validate().unwrap();
    }

    for status in ["", "99", "099", "600", "4a2", " 422"] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "errors": [{"status": status}]
        }))
        .unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidErrorStatus)
        );
    }
}

#[test]
fn validates_jsonapi_extension_and_profile_uris() {
    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "jsonapi": {
            "version": "1.1",
            "ext": ["https://jsonapi.org/ext/atomic"],
            "profile": ["urn:example:profile"]
        }
    }))
    .unwrap();
    valid.validate().unwrap();

    for jsonapi in [
        json!({"ext": ["relative/path"]}),
        json!({"profile": ["https://example.test/bad%2"]}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": null,
            "jsonapi": jsonapi
        }))
        .unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidJsonApiUri)
        );
    }
}

#[test]
fn jsonapi_extension_and_profile_members_are_uri_lists() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "jsonapi": {
            "ext": [
                "https://extensions.example.test/unknown",
                "urn:example:unrecognized-extension"
            ],
            "profile": [
                "https://profiles.example.test/unknown",
                "urn:example:unrecognized-profile"
            ]
        }
    }))
    .unwrap();
    document.validate().unwrap();

    for jsonapi in [
        json!({"ext": "https://extensions.example.test/unknown"}),
        json!({"profile": "https://profiles.example.test/unknown"}),
        json!({"ext": [1]}),
        json!({"profile": [false]}),
        json!({"ext": null}),
        json!({"profile": null}),
    ] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(json!({
                "data": null,
                "jsonapi": jsonapi
            }))
            .is_err()
        );
    }
}

#[test]
fn jsonapi_version_must_be_a_string_when_present() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "jsonapi": {"version": "1.1"}
    }))
    .unwrap();
    document.validate().unwrap();

    for version in [json!(1.1), json!(false), json!({}), json!([]), json!(null)] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(json!({
                "data": null,
                "jsonapi": {"version": version}
            }))
            .is_err()
        );
    }
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
fn link_objects_require_href_or_meta() {
    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "links": {
            "href-only": {"href": "/ports"},
            "meta-only": {"meta": {"count": 1}},
            "href-and-meta": {"href": "/ports", "meta": {"count": 1}}
        }
    }))
    .unwrap();
    valid.validate().unwrap();

    let missing_href_and_meta: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "links": {"self": {}}
    }))
    .unwrap();
    assert_eq!(
        missing_href_and_meta.validate(),
        Err(DocumentValidationError::InvalidLinkObject)
    );
}

#[test]
fn rejects_documents_without_data_errors_or_meta() {
    assert_eq!(
        JsonApiDocument::default().validate(),
        Err(DocumentValidationError::MissingContent)
    );
    let at_only: JsonApiDocument =
        serde_json::from_value(json!({"@documentAnnotation": false})).unwrap();
    assert_eq!(
        at_only.validate(),
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

    let at_only: JsonApiDocument =
        serde_json::from_value(json!({"errors": [{"@errorAnnotation": false}]})).unwrap();
    assert_eq!(
        at_only.validate(),
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
        json!({"data": null, "links": {"self": "bad uri"}}),
        json!({"data": null, "links": {"self": "/bad%2"}}),
        json!({"data": null, "links": {"bad relation": "/ports/1"}}),
        json!({"data": null, "links": {"self": {"href": "/ports/1", "rel": "not a relation"}}}),
        json!({"data": null, "links": {"self": {"href": "/ports/1", "rel": "bad/rel"}}}),
        json!({"data": {"type": "ports", "id": "1", "links": {"self": "bad uri"}}}),
        json!({"data": null, "links": {"self": {"href": "/ports/1", "hreflang": "en_US"}}}),
        json!({"data": null, "links": {"self": {"href": "/ports/1", "hreflang": []}}}),
        json!({"data": null, "links": {"self": {"href": "/ports/1", "hreflang": ["en", "bad--tag"]}}}),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": {"links": {"related": {"href": "bad uri"}}}}
            }
        }),
        json!({"errors": [{"links": {"about": {"href": "/errors/1", "hreflang": [1]}}}]}),
        json!({"errors": [{"links": {"about": "bad uri"}}]}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(document).unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidLinkObject)
        );
    }
}

#[test]
fn ignores_at_members_across_jsonapi_document_contexts() {
    let data_document: JsonApiDocument = serde_json::from_value(json!({
        "@documentAnnotation": false,
        "data": {
            "type": "ports",
            "id": "1",
            "@resourceAnnotation": false,
            "attributes": {"@attributeAnnotation": false, "name": "North"},
            "relationships": {
                "@relationshipAnnotation": false,
                "owner": {
                    "@relationshipObjectAnnotation": false,
                    "data": {
                        "type": "people",
                        "id": "2",
                        "@identifierAnnotation": false
                    },
                    "links": {
                        "@relationshipLink": false,
                        "related": {
                            "href": "/ports/1/owner",
                            "@linkObjectAnnotation": false
                        }
                    },
                    "meta": {"@relationshipMeta": false}
                }
            },
            "links": {
                "@resourceLink": false,
                "self": {"href": "/ports/1", "@linkObjectAnnotation": false}
            },
            "meta": {"@resourceMeta": false}
        },
        "links": {"@documentLink": false, "self": "/ports/1"},
        "meta": {"@documentMeta": false},
        "jsonapi": {
            "@jsonapiAnnotation": false,
            "version": "1.1",
            "meta": {"@jsonapiMeta": false}
        }
    }))
    .unwrap();
    data_document.validate().unwrap();
    let Some(PrimaryData::One(resource)) = data_document.data.as_ref() else {
        panic!("expected a resource object");
    };
    let attributes = resource.attributes.as_ref().unwrap();
    assert_eq!(attributes.len(), 1);
    assert_eq!(attributes.get("name"), Some(&json!("North")));

    let error_document: JsonApiDocument = serde_json::from_value(json!({
        "@documentAnnotation": false,
        "errors": [{
            "@errorAnnotation": false,
            "detail": "not found",
            "links": {"@errorLink": false, "about": "/errors/1"},
            "source": {"pointer": "/data", "@sourceAnnotation": false},
            "meta": {"@errorMeta": false}
        }],
        "meta": {"@documentMeta": false}
    }))
    .unwrap();
    error_document.validate().unwrap();

    let invalid_ordinary_link: JsonApiDocument =
        serde_json::from_value(json!({"data": null, "links": {"self": false}})).unwrap();
    assert_eq!(
        invalid_ordinary_link.validate(),
        Err(DocumentValidationError::InvalidLinkObject)
    );
}

#[test]
fn validates_uri_references_and_registered_or_extension_link_relations() {
    for href in [
        "",
        "/ports/1",
        "../ports/1",
        "?include=owner",
        "#details",
        "//example.test/ports/1",
        "https://example.test/ports/1",
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": null,
            "links": {
                "alternate": href,
                "https://example.test/rels/related": {
                    "href": "/ports/1",
                    "rel": "https://example.test/rels/related"
                }
            }
        }))
        .unwrap();
        document.validate().unwrap();
    }
}

#[test]
fn validates_link_object_media_type_hints() {
    for media_type in [
        "application/vnd.api+json",
        "application/problem+json",
        "text/plain; charset=utf-8",
        "text/plain; note=\"a;b\"",
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": null,
            "links": {"describedby": {"href": "/schema", "type": media_type}}
        }))
        .unwrap();
        document.validate().unwrap();
    }

    for media_type in [
        "plain",
        "application/",
        "/json",
        "application/*",
        "*/json",
        "*/vnd.api+json",
        "*/*",
        "application/*+json",
        "text/plain; charset=",
        "text/plain; =utf-8",
        "text/plain; charset",
        "text/plain; charset=\"unterminated",
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": null,
            "links": {"describedby": {"href": "/schema", "type": media_type}}
        }))
        .unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::InvalidLinkObject),
            "{media_type}"
        );
    }
}

#[test]
fn accepts_well_formed_bcp47_language_tags() {
    for hreflang in [
        json!("en"),
        json!("en-US"),
        json!("zh-Hant"),
        json!("de-CH-1901"),
        json!(["en", "fr-CA"]),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": null,
            "links": {"describedby": {"href": "/schema", "hreflang": hreflang}}
        }))
        .unwrap();
        document.validate().unwrap();
    }
}

#[test]
fn rejects_well_formed_but_unregistered_bcp47_language_tags() {
    let unregistered = LanguageTag::parse("en-foobar").unwrap();
    assert!(unregistered.validate().is_err());

    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": null,
        "links": {"describedby": {"href": "/schema", "hreflang": "en-foobar"}}
    }))
    .unwrap();
    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::InvalidLinkObject)
    );
}

#[test]
fn rejects_included_resources_without_primary_data() {
    for value in [
        json!({"included": [{"type": "ports", "id": "1"}]}),
        json!({
            "errors": [{"title": "Request failed"}],
            "included": [{"type": "ports", "id": "1"}]
        }),
    ] {
        let document: JsonApiDocument = serde_json::from_value(value).unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::IncludedWithoutData)
        );
    }
}

#[test]
fn empty_primary_collection_allows_only_empty_included_array() {
    let empty_compound_document: JsonApiDocument =
        serde_json::from_value(json!({"data": [], "included": []})).unwrap();
    empty_compound_document.validate_response().unwrap();

    let unreachable: JsonApiDocument = serde_json::from_value(json!({
        "data": [],
        "included": [{"type": "ports", "id": "1"}]
    }))
    .unwrap();
    assert_eq!(
        unreachable.validate(),
        Err(DocumentValidationError::UnreachableIncludedResource)
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
fn validates_included_resources_reachable_from_any_collection_member() {
    let mut second_primary = resource("ports", "2");
    second_primary.relationships = Some(BTreeMap::from([(
        "owner".to_owned(),
        relationship_to("people", "3"),
    )]));
    let document = JsonApiDocument {
        data: Some(PrimaryData::Many(vec![
            resource("ports", "1"),
            second_primary,
        ])),
        included: Some(vec![resource("people", "3")]),
        ..JsonApiDocument::default()
    };

    document.validate().unwrap();
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
fn relationship_identifiers_require_type_and_exactly_one_identity() {
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
    valid.validate().unwrap();

    let missing_type: Result<JsonApiDocument, _> = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"id": "2"}}
            }
        }
    }));
    assert!(missing_type.is_err());

    let missing_identity: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people"}}
            }
        }
    }))
    .unwrap();
    assert_eq!(
        missing_identity.validate(),
        Err(DocumentValidationError::MissingIdentifier)
    );

    let duplicate_identity: JsonApiDocument = serde_json::from_value(json!({
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
        duplicate_identity.validate(),
        Err(DocumentValidationError::BothIdentifiers)
    );
}

#[test]
fn resource_fields_must_not_conflict_with_type_id_or_each_other() {
    let separate_names: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "attributes": {"name": "Harbor"},
            "relationships": {"owner": {"data": null}}
        }
    }))
    .unwrap();
    separate_names.validate().unwrap();

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
            "relationships": {"related": {"data": null}}
        }
    }))
    .unwrap();
    valid.validate().unwrap();
}

#[test]
fn ignores_at_members_when_interpreting_resource_relationships() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "attributes": {"@owner": "annotation"},
            "relationships": {"@owner": true}
        }
    }))
    .unwrap();
    document.validate().unwrap();
    let Some(PrimaryData::One(parsed_resource)) = document.data else {
        panic!("expected a resource object");
    };
    assert!(parsed_resource.relationships.is_none());
    assert!(parsed_resource.attributes.is_none());

    let mut resource = resource("ports", "1");
    resource.attributes = Some(serde_json::Map::from_iter([(
        "@owner".to_owned(),
        json!("annotation"),
    )]));
    resource.relationships = Some(BTreeMap::from([(
        "@owner".to_owned(),
        Relationship::default(),
    )]));
    JsonApiDocument {
        data: Some(PrimaryData::One(resource)),
        ..JsonApiDocument::default()
    }
    .validate()
    .unwrap();

    let invalid_relationship: Result<JsonApiDocument, _> = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {"owner": true}
        }
    }));
    assert!(invalid_relationship.is_err());
}

#[test]
fn rejects_invalid_at_member_names_while_ignoring_valid_members() {
    for document in [
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "attributes": {"@bad/": "ignored"}
            }
        }),
        json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"@bad/": "ignored"}
            }
        }),
    ] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(document).is_err(),
            "invalid @-member names must be rejected before their values are ignored"
        );
    }

    let invalid_link_relation: Result<JsonApiDocument, _> =
        serde_json::from_value(json!({"links": {"@bad/": false}}));
    assert!(invalid_link_relation.is_err());

    let valid: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "attributes": {"@valid-name": {"arbitrary": ["annotation"]}},
            "relationships": {"@valid-name": false}
        },
        "links": {"@valid-name": false}
    }))
    .unwrap();
    valid.validate().unwrap();
}

#[test]
fn ignores_valid_at_members_and_rejects_invalid_names_in_object_maps() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {
                    "data": null,
                    "links": {"@annotation": false, "related": "/ports/1/owner"},
                    "meta": {"@annotation": false, "source": "fixture"}
                }
            },
            "links": {"@annotation": false, "self": "/ports/1"},
            "meta": {"@annotation": false, "visible": true}
        },
        "links": {"@annotation": false, "self": {
            "href": "/ports",
            "@linkAnnotation": false,
            "meta": {"@metaAnnotation": false, "visible": true},
            "describedby": {"href": "/schema", "@describedbyAnnotation": false}
        }},
        "meta": {"@annotation": false, "revision": "one"},
        "jsonapi": {"version": "1.1", "meta": {"@annotation": false, "vendor": "fixture"}}
    }))
    .unwrap();
    document.validate().unwrap();
    assert_eq!(document.meta.as_ref().unwrap().len(), 1);
    assert_eq!(
        document.meta.as_ref().unwrap().get("revision"),
        Some(&json!("one"))
    );
    assert_eq!(document.links.as_ref().unwrap().len(), 1);
    assert_eq!(
        document.links.as_ref().unwrap().get("self"),
        Some(&json!({
            "href": "/ports",
            "meta": {"visible": true},
            "describedby": {"href": "/schema"}
        }))
    );
    let Some(PrimaryData::One(resource)) = document.data.as_ref() else {
        panic!("expected one primary resource");
    };
    assert_eq!(resource.links.as_ref().unwrap().len(), 1);
    assert_eq!(resource.meta.as_ref().unwrap().len(), 1);
    assert_eq!(
        resource.meta.as_ref().unwrap().get("visible"),
        Some(&json!(true))
    );
    let relationship = resource
        .relationships
        .as_ref()
        .unwrap()
        .get("owner")
        .unwrap();
    assert_eq!(relationship.links.as_ref().unwrap().len(), 1);
    assert_eq!(relationship.meta.as_ref().unwrap().len(), 1);
    assert_eq!(
        relationship.meta.as_ref().unwrap().get("source"),
        Some(&json!("fixture"))
    );
    assert_eq!(
        document
            .jsonapi
            .as_ref()
            .unwrap()
            .meta
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    let annotation_only: JsonApiDocument =
        serde_json::from_value(json!({"meta": {"@annotation": false}})).unwrap();
    assert!(annotation_only.meta.is_none());
    let empty_meta: JsonApiDocument = serde_json::from_value(json!({"meta": {}})).unwrap();
    assert_eq!(empty_meta.meta, Some(Default::default()));

    for value in [
        json!({"meta": {"@bad/": false}}),
        json!({"links": {"@bad/": false}}),
        json!({"links": {"self": {"href": "/ports", "@bad/": false}}}),
        json!({"links": {"self": {"href": "/ports", "meta": {"@bad/": false}}}}),
        json!({"links": {"self": {"href": "/ports", "describedby": {
            "href": "/schema", "@bad/": false
        }}}}),
        json!({"data": {"type": "ports", "id": "1", "meta": {"@bad/": false}}}),
        json!({"data": {"type": "ports", "id": "1", "links": {"@bad/": false}}}),
        json!({"data": {"type": "ports", "id": "1", "relationships": {"owner": {
            "data": null, "meta": {"@bad/": false}
        }}}}),
        json!({"errors": [{"detail": "missing", "meta": {"@bad/": false}}]}),
        json!({"jsonapi": {"meta": {"@bad/": false}}}),
    ] {
        assert!(
            serde_json::from_value::<JsonApiDocument>(value).is_err(),
            "invalid @-member names must be rejected in metadata maps"
        );
    }
}

#[test]
fn relationship_objects_require_linkage_links_or_metadata() {
    for relationship in [
        json!({}),
        json!({"links": {}}),
        json!({"links": {"@relationshipLink": false}}),
        json!({"@relationshipAnnotation": false}),
    ] {
        let document: JsonApiDocument = serde_json::from_value(json!({
            "data": {
                "type": "ports",
                "id": "1",
                "relationships": {"owner": relationship}
            }
        }))
        .unwrap();
        assert_eq!(
            document.validate(),
            Err(DocumentValidationError::EmptyRelationship)
        );
    }

    for relationship in [
        json!({"links": {"related": "/ports/1/owner"}}),
        json!({"meta": {}}),
        json!({"data": null, "links": {}}),
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
        included: Some(vec![local_resource("people", "new-owner")]),
        ..JsonApiDocument::default()
    };

    document.validate().unwrap();
}

#[test]
fn rejects_relationship_local_ids_without_matching_resource_objects() {
    let document: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people", "lid": "missing-owner"}}
            }
        }
    }))
    .unwrap();
    assert_eq!(
        document.validate(),
        Err(DocumentValidationError::UnresolvedLocalIdentifier)
    );

    let mismatched_type: JsonApiDocument = serde_json::from_value(json!({
        "data": {
            "type": "ports",
            "id": "1",
            "relationships": {
                "owner": {"data": {"type": "people", "lid": "owner"}}
            }
        },
        "included": [{"type": "authors", "lid": "owner"}]
    }))
    .unwrap();
    assert_eq!(
        mismatched_type.validate(),
        Err(DocumentValidationError::UnresolvedLocalIdentifier)
    );
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

    let mut primary_with_relationship = resource("ports", "1");
    primary_with_relationship.relationships = Some(BTreeMap::from([(
        "owner".to_owned(),
        relationship_to("people", "1"),
    )]));
    let same_id_on_different_types_across_primary_and_included = JsonApiDocument {
        data: Some(PrimaryData::One(primary_with_relationship)),
        included: Some(vec![resource("people", "1")]),
        ..JsonApiDocument::default()
    };
    same_id_on_different_types_across_primary_and_included
        .validate()
        .unwrap();

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
