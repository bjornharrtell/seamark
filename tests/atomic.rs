#![allow(missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::http::HeaderMap;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use seamark::atomic::{
    AtomicExecutionError, AtomicHrefResolver, AtomicOperationHandler, AtomicOperationOutcome,
    AtomicOperationsDocument, AtomicOperationsError, AtomicOperationsGuard, AtomicResourceData,
    AtomicResourceReference, AtomicResult, LocalIdMap, PlannedAtomicOperation, PlannedOperation,
    execute_atomic_operations, plan_atomic_operations, plan_atomic_operations_with_href_resolver,
};
use seamark::document::{RelationshipData, ResourceIdentifier};
use seamark::registry::{ResourceDefinition, ResourceRegistry};
use serde_json::{Value, json};

fn registry() -> ResourceRegistry {
    let authors =
        ResourceDefinition::new("authors", "author_id").attribute("name", "name", false, false);
    let articles = ResourceDefinition::new("articles", "article_id")
        .attribute("title", "title", false, false)
        .relationship("author", "author_id", "authors")
        .relationship("tags", "tag_ids", "tags");
    let tags = ResourceDefinition::new("tags", "tag_id").attribute("name", "name", false, false);
    ResourceRegistry::new([authors, articles, tags]).unwrap()
}

fn document(value: Value) -> AtomicOperationsDocument {
    serde_json::from_value(value).unwrap()
}

fn plan(value: Value) -> Result<Vec<PlannedAtomicOperation>, AtomicOperationsError> {
    plan_atomic_operations(&registry(), &document(value))
}

struct TestHrefResolver;

impl AtomicHrefResolver for TestHrefResolver {
    fn resolve_relationship(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if href == "/articles/7/relationships/author" {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some("7".to_owned()),
                lid: None,
                relationship: Some("author".to_owned()),
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_resource(&self, href: &str) -> Result<Option<AtomicResourceReference>, String> {
        if href == "/articles/7" {
            Ok(Some(AtomicResourceReference {
                type_name: "articles".to_owned(),
                id: Some("7".to_owned()),
                lid: None,
                relationship: None,
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_collection(&self, href: &str) -> Result<Option<String>, String> {
        Ok((href == "/articles").then(|| "articles".to_owned()))
    }
}

#[test]
fn plans_ordered_resource_and_relationship_operations_with_local_ids() {
    let planned = plan(json!({
        "atomic:operations": [
            {
                "op": "add",
                "data": {
                    "type": "authors",
                    "lid": "author-local",
                    "attributes": {"name": "Ada"}
                }
            },
            {
                "op": "add",
                "data": {
                    "type": "articles",
                    "lid": "article-local",
                    "attributes": {"title": "Systems"},
                    "relationships": {
                        "author": {"data": {"type": "authors", "lid": "author-local"}}
                    }
                }
            },
            {
                "op": "update",
                "ref": {"type": "articles", "lid": "article-local"},
                "data": {"type": "articles", "lid": "article-local", "attributes": {"title": "Systems 2"}}
            },
            {
                "op": "update",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "author"},
                "data": null
            },
            {
                "op": "add",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "tags"},
                "data": [{"type": "tags", "id": "tag-1"}]
            },
            {
                "op": "remove",
                "ref": {"type": "articles", "lid": "article-local", "relationship": "tags"},
                "data": [{"type": "tags", "id": "tag-1"}]
            },
            {
                "op": "remove",
                "ref": {"type": "articles", "lid": "article-local"}
            }
        ]
    }))
    .unwrap();
    assert_eq!(planned.len(), 7);
    assert!(matches!(
        planned[0].operation,
        PlannedOperation::AddResource { .. }
    ));
    assert!(matches!(
        planned[3].operation,
        PlannedOperation::UpdateRelationship {
            data: RelationshipData::Null,
            ..
        }
    ));
    assert!(matches!(
        planned[4].operation,
        PlannedOperation::AddRelationshipMembers { .. }
    ));
    assert!(matches!(
        planned[5].operation,
        PlannedOperation::RemoveRelationshipMembers { .. }
    ));
    assert!(matches!(
        planned[6].operation,
        PlannedOperation::RemoveResource { .. }
    ));
}

#[test]
fn accepts_server_assigned_resource_ids_and_preserves_omitted_vs_null_data() {
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "attributes": {"name": "Ada"}}},
            {
                "op": "update",
                "ref": {"type": "articles", "id": "1", "relationship": "author"},
                "data": null
            }
        ]
    }))
    .unwrap();
    let PlannedOperation::AddResource { data, .. } = &operations[0].operation else {
        panic!("expected an add-resource operation");
    };
    assert_eq!(data.id, None);
    assert_eq!(data.lid, None);

    let parsed = document(json!({
        "atomic:operations": [
            {"op": "update", "ref": {"type": "articles", "id": "1", "relationship": "author"}, "data": null},
            {"op": "remove", "ref": {"type": "articles", "id": "1"}}
        ]
    }));
    assert_eq!(
        parsed.operations.as_ref().unwrap()[0].data,
        Some(Value::Null)
    );
    assert_eq!(parsed.operations.as_ref().unwrap()[1].data, None);
}

#[test]
fn maps_resource_and_relationship_changesets_to_internal_fields() {
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author-local", "attributes": {"name": "Ada"}}},
            {
                "op": "update",
                "ref": {"type": "articles", "id": "1"},
                "data": {
                    "type": "articles",
                    "id": "1",
                    "attributes": {"title": null},
                    "relationships": {
                        "author": {"data": {"type": "authors", "lid": "author-local"}},
                        "tags": {"meta": {}}
                    }
                }
            },
            {
                "op": "update",
                "ref": {"type": "articles", "id": "1", "relationship": "author"},
                "data": null
            },
            {"op": "update", "ref": {"type": "articles", "id": "2"}, "data": {"type": "articles", "id": "2"}}
        ]
    }))
    .unwrap();

    let PlannedOperation::AddResource { changeset, .. } = &operations[0].operation else {
        panic!("expected an add-resource operation");
    };
    assert_eq!(
        changeset.attributes.as_ref().unwrap().get("name"),
        Some(&json!("Ada"))
    );

    let PlannedOperation::UpdateResource { changeset, .. } = &operations[1].operation else {
        panic!("expected an update-resource operation");
    };
    assert_eq!(changeset.identifier_field, "article_id");
    assert_eq!(
        changeset.attributes.as_ref().unwrap().get("title"),
        Some(&Value::Null)
    );
    let relationship = changeset
        .relationships
        .as_ref()
        .unwrap()
        .get("author_id")
        .unwrap();
    assert_eq!(
        relationship.data,
        Some(RelationshipData::One(ResourceIdentifier {
            type_name: "authors".to_owned(),
            lid: Some("author-local".to_owned()),
            ..ResourceIdentifier::default()
        }))
    );
    assert_eq!(
        changeset.relationships.as_ref().unwrap()["tag_ids"].data,
        None
    );

    let PlannedOperation::UpdateRelationship {
        model_field, data, ..
    } = &operations[2].operation
    else {
        panic!("expected an update-relationship operation");
    };
    assert_eq!(model_field, "author_id");
    assert_eq!(data, &RelationshipData::Null);

    let PlannedOperation::UpdateResource { changeset, .. } = &operations[3].operation else {
        panic!("expected an update-resource operation");
    };
    assert_eq!(changeset.attributes, None);
    assert_eq!(changeset.relationships, None);
}

#[test]
fn atomic_documents_and_nested_objects_must_be_json_objects() {
    for value in [
        json!([[{"op": "remove", "ref": {"type": "authors", "id": "1"}}]]),
        json!({"atomic:operations": [["remove"]]}),
        json!({"atomic:operations": [{"op": "remove", "ref": ["authors", "1"]}]}),
        json!({"atomic:results": [[]]}),
    ] {
        assert!(
            serde_json::from_value::<AtomicOperationsDocument>(value.clone()).is_err(),
            "expected Atomic object shapes to reject array values: {value}"
        );
    }

    let request: AtomicOperationsDocument = serde_json::from_value(json!({
        "atomic:operations": [{"op": "add", "data": ["authors"]}]
    }))
    .unwrap();
    assert!(plan_atomic_operations(&registry(), &request).is_err());
}

#[test]
fn accepts_uri_reference_targets_for_resource_mutations() {
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "href": "/author-collection", "data": {"type": "authors", "attributes": {"name": "Ada"}}},
            {"op": "update", "href": "/author-resource/1", "data": {"type": "authors", "id": "1", "attributes": {"name": "Ada Lovelace"}}},
            {"op": "remove", "href": "/author-resource/1"}
        ]
    }))
    .unwrap();
    assert_eq!(operations.len(), 3);

    assert!(
        plan(json!({
            "atomic:operations": [
                {"op": "remove", "href": "not a URI reference"}
            ]
        }))
        .is_err()
    );
}

#[test]
fn atomic_operations_must_choose_reference_or_href_target() {
    assert_eq!(
        plan(json!({
            "atomic:operations": [{
                "op": "remove",
                "ref": {"type": "authors", "id": "1"},
                "href": "/author-resource/1"
            }]
        })),
        Err(AtomicOperationsError::InvalidOperation {
            index: 0,
            pointer: "/atomic:operations/0".to_owned(),
            message: "an operation must not contain both `ref` and `href`".to_owned(),
        })
    );
}

#[test]
fn validates_request_response_shapes_and_result_cardinality() {
    assert_eq!(
        AtomicOperationsDocument::default().validate_request(),
        Err(AtomicOperationsError::MissingOperations)
    );
    assert_eq!(
        AtomicOperationsDocument::default().validate_response(0),
        Err(AtomicOperationsError::MissingResults)
    );
    assert_eq!(
        document(json!({"atomic:results": []})).validate_request(),
        Err(AtomicOperationsError::InvalidDocument(
            "an operations request must not contain `atomic:results`"
        ))
    );
    assert_eq!(
        document(json!({"atomic:operations": [], "errors": [{"title": "bad"}]})).validate_request(),
        Err(AtomicOperationsError::InvalidDocument(
            "an operations request must not contain `errors`"
        ))
    );
    assert_eq!(
        document(json!({"atomic:results": [], "errors": [{"title": "bad"}]})).validate_response(0),
        Err(AtomicOperationsError::InvalidDocument(
            "an operations response must not contain `errors`"
        ))
    );
    for value in [
        json!({"data": null, "atomic:operations": []}),
        json!({"included": [], "atomic:operations": []}),
    ] {
        assert_eq!(
            document(value).validate_request(),
            Err(AtomicOperationsError::InvalidDocument(
                "an Atomic Operations document must not contain `data` or `included`"
            ))
        );
    }
    assert_eq!(
        document(json!({"atomic:results": [{"data": null}, {}]})).validate_response(1),
        Err(AtomicOperationsError::ResultCountMismatch {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(
        document(json!({"atomic:results": [{}]})).validate_response(2),
        Err(AtomicOperationsError::ResultCountMismatch {
            expected: 2,
            actual: 1
        })
    );
    assert_eq!(
        document(json!({"atomic:results": [{}, {}]}))
            .validate_response(2)
            .unwrap()
            .len(),
        2
    );
    assert!(
        serde_json::from_value::<AtomicOperationsDocument>(json!({
            "data": null,
            "atomic:operations": []
        }))
        .unwrap()
        .validate_request()
        .is_err()
    );
}

#[test]
fn accepts_empty_atomic_operations_as_a_valid_no_op() {
    let request = document(json!({"atomic:operations": []}));

    assert!(request.validate_request().unwrap().is_empty());
    assert!(
        plan_atomic_operations(&registry(), &request)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn validates_atomic_resource_result_order() {
    let operations = plan(json!({
        "atomic:operations": [
            {
                "op": "update",
                "ref": {"type": "authors", "id": "1"},
                "data": {"type": "authors", "attributes": {"name": "First"}}
            },
            {
                "op": "update",
                "ref": {"type": "authors", "id": "2"},
                "data": {"type": "authors", "attributes": {"name": "Second"}}
            }
        ]
    }))
    .unwrap();

    let ordered = document(json!({
        "atomic:results": [
            {"data": {"type": "authors", "id": "1"}},
            {"data": {"type": "authors", "id": "2"}}
        ]
    }));
    assert_eq!(ordered.validate_response_for(&operations).unwrap().len(), 2);

    let swapped = document(json!({
        "atomic:results": [
            {"data": {"type": "authors", "id": "2"}},
            {"data": {"type": "authors", "id": "1"}}
        ]
    }));
    assert!(matches!(
        swapped.validate_response_for(&operations),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));
}

#[test]
fn validates_base_jsonapi_members_in_atomic_documents_and_resource_data() {
    for value in [
        json!({"atomic:operations": [], "links": {"self": {}}}),
        json!({"atomic:operations": [], "jsonapi": {"profile": ["relative/profile"]}}),
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {"type": "authors", "links": {"self": {}}}
            }]
        }),
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {"type": "articles", "relationships": {"author": {}}}
            }]
        }),
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {
                    "type": "articles",
                    "relationships": {"author": {"links": {"related": {}}}}
                }
            }]
        }),
    ] {
        assert!(plan(value).is_err());
    }

    plan(json!({
        "jsonapi": {"version": "1.1"},
        "links": {"self": "/operations"},
        "atomic:operations": [{
            "op": "add",
            "data": {
                "type": "articles",
                "relationships": {"author": {"meta": {}}}
            }
        }]
    }))
    .unwrap();
}

#[test]
fn validates_atomic_result_data_against_operation_kind_and_resource_rules() {
    let add = plan(json!({
        "atomic:operations": [{"op": "add", "data": {"type": "authors"}}]
    }))
    .unwrap();
    let valid_resource_result = document(json!({
        "atomic:results": [{"data": {"type": "authors", "id": "1"}}]
    }));
    valid_resource_result.validate_response_for(&add).unwrap();
    let missing_resource_result = document(json!({"atomic:results": [{}]}));
    assert!(matches!(
        missing_resource_result.validate_response_for(&add),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));

    let missing_id = document(json!({
        "atomic:results": [{"data": {"type": "authors", "lid": "local"}}]
    }));
    assert!(matches!(
        missing_id.validate_response_for(&add),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));
    let mismatched_type = document(json!({
        "atomic:results": [{"data": {"type": "tags", "id": "1"}}]
    }));
    assert!(matches!(
        mismatched_type.validate_response_for(&add),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));

    let relationship_update = plan(json!({
        "atomic:operations": [{
            "op": "update",
            "ref": {"type": "articles", "id": "1", "relationship": "author"},
            "data": null
        }]
    }))
    .unwrap();
    let unexpected_data = document(json!({
        "atomic:results": [{"data": {"type": "authors", "id": "2"}}]
    }));
    assert!(matches!(
        unexpected_data.validate_response_for(&relationship_update),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));

    let empty_relationship_result = document(json!({"atomic:results": [{}]}));
    empty_relationship_result
        .validate_response_for(&relationship_update)
        .unwrap();

    let remove = plan(json!({
        "atomic:operations": [{
            "op": "remove",
            "ref": {"type": "authors", "id": "1"}
        }]
    }))
    .unwrap();
    let unexpected_remove_data = document(json!({
        "atomic:results": [{"data": {"type": "authors", "id": "1"}}]
    }));
    assert!(matches!(
        unexpected_remove_data.validate_response_for(&remove),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));
    document(json!({"atomic:results": [{}]}))
        .validate_response_for(&remove)
        .unwrap();
}

#[test]
fn validates_relationship_results_for_add_update_and_remove_operations() {
    let operations = plan(json!({
        "atomic:operations": [
            {
                "op": "add",
                "ref": {"type": "articles", "id": "1", "relationship": "tags"},
                "data": [{"type": "tags", "id": "2"}]
            },
            {
                "op": "update",
                "ref": {"type": "articles", "id": "1", "relationship": "author"},
                "data": null
            },
            {
                "op": "remove",
                "ref": {"type": "articles", "id": "1", "relationship": "tags"},
                "data": [{"type": "tags", "id": "2"}]
            }
        ]
    }))
    .unwrap();
    let valid_results = document(json!({
        "atomic:results": [
            {"meta": {"changed": true}},
            {},
            {}
        ]
    }));
    valid_results.validate_response_for(&operations).unwrap();

    for (index, data) in [
        json!([{"type": "tags", "id": "2"}]),
        json!(null),
        json!([{"type": "tags", "id": "2"}]),
    ]
    .into_iter()
    .enumerate()
    {
        let invalid_results = document(json!({
            "atomic:results": [
                {"meta": {"changed": true}},
                {},
                {}
            ]
        }));
        let mut invalid_results = invalid_results;
        invalid_results.results.as_mut().unwrap()[index].data = Some(data);
        assert!(matches!(
            invalid_results.validate_response_for(&operations),
            Err(AtomicOperationsError::InvalidResult {
                index: result_index,
                ..
            }) if result_index == index
        ));
    }
}

#[test]
fn validates_atomic_client_assigned_add_result_identity() {
    let add = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "id": "requested", "attributes": {"name": "Ada"}}
        }]
    }))
    .unwrap();

    document(json!({
        "atomic:results": [{
            "data": {"type": "authors", "id": "requested", "attributes": {"name": "Ada"}}
        }]
    }))
    .validate_response_for(&add)
    .unwrap();

    let mismatched_id = document(json!({
        "atomic:results": [{"data": {"type": "authors", "id": "different"}}]
    }));
    assert!(matches!(
        mismatched_id.validate_response_for(&add),
        Err(AtomicOperationsError::InvalidResult { index: 0, .. })
    ));
}

#[test]
fn validates_atomic_resource_update_result_shapes() {
    let update = plan(json!({
        "atomic:operations": [{
            "op": "update",
            "ref": {"type": "articles", "id": "1"},
            "data": {"type": "articles", "attributes": {"title": "Updated"}}
        }]
    }))
    .unwrap();

    document(json!({"atomic:results": [{}]}))
        .validate_response_for(&update)
        .unwrap();
    document(json!({
        "atomic:results": [{
            "data": {
                "type": "articles",
                "id": "1",
                "attributes": {"title": "Updated"}
            }
        }]
    }))
    .validate_response_for(&update)
    .unwrap();

    for data in [
        json!({"type": "articles", "id": "2"}),
        json!({"type": "articles", "lid": "local"}),
    ] {
        let mismatched_or_nonpersistent_identity =
            document(json!({"atomic:results": [{"data": data}]}));
        assert!(matches!(
            mismatched_or_nonpersistent_identity.validate_response_for(&update),
            Err(AtomicOperationsError::InvalidResult { index: 0, .. })
        ));
    }
}

#[test]
fn ignores_unrecognized_members_in_atomic_documents_and_operation_objects() {
    let operations = plan(json!({
        "future:document": {"ignored": true},
        "atomic:operations": [{
            "op": "add",
            "future:operation": "ignored",
            "data": {
                "type": "authors",
                "attributes": {"name": "Ada"},
                "future:resource": "ignored"
            }
        }, {
            "op": "remove",
            "ref": {
                "type": "articles",
                "id": "1",
                "future:reference": "ignored"
            }
        }]
    }))
    .unwrap();
    assert_eq!(operations.len(), 2);

    let response = document(json!({
        "atomic:results": [{"future:result": true}],
        "future:document": true
    }));
    assert_eq!(response.validate_response(1).unwrap().len(), 1);
}

#[test]
fn rejects_invalid_at_member_names_in_atomic_protocol_objects() {
    for (index, value) in [
        json!({"@bad/": false, "atomic:operations": []}),
        json!({
            "atomic:operations": [{
                "@bad/": false,
                "op": "remove",
                "ref": {"type": "authors", "id": "1"}
            }]
        }),
        json!({
            "atomic:operations": [{
                "op": "add",
                "data": {
                    "@bad/": false,
                    "type": "authors"
                }
            }]
        }),
        json!({
            "atomic:operations": [{
                "op": "remove",
                "ref": {"type": "authors", "id": "1", "@bad/": false}
            }]
        }),
        json!({"atomic:results": [{"@bad/": false}]}),
    ]
    .into_iter()
    .enumerate()
    {
        let rejected = match serde_json::from_value::<AtomicOperationsDocument>(value) {
            Ok(document) => plan_atomic_operations(&registry(), &document).is_err(),
            Err(_) => true,
        };
        assert!(
            rejected,
            "invalid @-member names must be rejected before unknown members are ignored (case {index})"
        );
    }
}

#[test]
fn ignores_valid_at_members_and_rejects_invalid_names_in_atomic_object_maps() {
    let document: AtomicOperationsDocument = serde_json::from_value(json!({
        "links": {"@annotation": false, "self": {
            "href": "/operations",
            "@linkAnnotation": false,
            "meta": {"@metaAnnotation": false, "scope": "request"}
        }},
        "meta": {"@annotation": false, "request": "kept"},
        "atomic:operations": [{
            "op": "add",
            "meta": {"@annotation": false, "operation": "kept"},
            "data": {
                "type": "authors",
                "links": {"@annotation": false, "self": {
                    "href": "/authors/1",
                    "@linkAnnotation": false
                }},
                "meta": {"@annotation": false, "resource": "kept"}
            }
        }]
    }))
    .unwrap();
    assert_eq!(document.links.as_ref().unwrap().len(), 1);
    assert_eq!(
        document.links.as_ref().unwrap().get("self"),
        Some(&json!({"href": "/operations", "meta": {"scope": "request"}}))
    );
    assert_eq!(document.meta.as_ref().unwrap().len(), 1);
    assert_eq!(
        document.meta.as_ref().unwrap().get("request"),
        Some(&json!("kept"))
    );
    assert_eq!(
        document.operations.as_ref().unwrap()[0]
            .meta
            .as_ref()
            .unwrap()
            .get("operation"),
        Some(&json!("kept"))
    );
    let resource_data: AtomicResourceData = serde_json::from_value(
        document.operations.as_ref().unwrap()[0]
            .data
            .as_ref()
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_eq!(resource_data.links.as_ref().unwrap().len(), 1);
    assert_eq!(
        resource_data.links.as_ref().unwrap().get("self"),
        Some(&json!({"href": "/authors/1"}))
    );
    assert_eq!(
        resource_data.meta.as_ref().unwrap().get("resource"),
        Some(&json!("kept"))
    );

    for value in [
        json!({"links": {"@bad/": false}, "atomic:operations": []}),
        json!({"links": {"self": {"href": "/operations", "@bad/": false}}, "atomic:operations": []}),
        json!({"meta": {"@bad/": false}, "atomic:operations": []}),
        json!({"atomic:operations": [{
            "op": "remove",
            "ref": {"type": "authors", "id": "1"},
            "meta": {"@bad/": false}
        }]}),
        json!({"atomic:results": [{"meta": {"@bad/": false}}]}),
    ] {
        assert!(
            serde_json::from_value::<AtomicOperationsDocument>(value).is_err(),
            "invalid @-member names must be rejected in Atomic metadata maps"
        );
    }

    let invalid_resource_metadata = json!({
        "type": "authors",
        "meta": {"@bad/": false}
    });
    assert!(serde_json::from_value::<AtomicResourceData>(invalid_resource_metadata).is_err());
    let invalid_resource_links = json!({
        "type": "authors",
        "links": {"@bad/": false}
    });
    assert!(serde_json::from_value::<AtomicResourceData>(invalid_resource_links).is_err());
    let invalid_resource_link_object = json!({
        "type": "authors",
        "links": {"self": {"href": "/authors/1", "@bad/": false}}
    });
    assert!(serde_json::from_value::<AtomicResourceData>(invalid_resource_link_object).is_err());
    let invalid_operation_data = json!({"atomic:operations": [{
        "op": "add",
        "data": {"type": "authors", "meta": {"@bad/": false}}
    }]});
    let document: AtomicOperationsDocument =
        serde_json::from_value(invalid_operation_data).unwrap();
    assert!(plan_atomic_operations(&registry(), &document).is_err());
}

#[test]
fn ignores_at_members_when_planning_atomic_resource_data() {
    let operations = plan(json!({
        "@documentAnnotation": false,
        "meta": {"@documentMeta": false},
        "atomic:operations": [{
            "@operationAnnotation": false,
            "op": "add",
            "data": {
                "@resourceAnnotation": false,
                "type": "authors",
                "attributes": {"@attributeAnnotation": false, "name": "Ada"},
                "relationships": {"@relationshipAnnotation": false},
                "links": {"@resourceLink": false, "self": "/authors/1"},
                "meta": {"@resourceMeta": false}
            }
        }, {
            "@operationAnnotation": false,
            "op": "remove",
            "ref": {
                "type": "authors",
                "id": "1",
                "@referenceAnnotation": false
            }
        }]
    }))
    .unwrap();
    assert_eq!(operations.len(), 2);
    let PlannedOperation::AddResource { changeset, .. } = &operations[0].operation else {
        panic!("expected an add-resource operation");
    };
    let attributes = changeset.attributes.as_ref().unwrap();
    assert_eq!(attributes.len(), 1);
    assert_eq!(attributes.get("name"), Some(&json!("Ada")));
    assert!(changeset.relationships.is_none());

    assert!(
        plan(json!({
            "atomic:operations": [{
                "op": "add",
                "data": {"type": "authors", "attributes": {"unknown": "not ignored"}}
            }]
        }))
        .is_err()
    );

    let response = document(json!({
        "atomic:results": [{"@resultAnnotation": false}]
    }));
    assert_eq!(response.validate_response(1).unwrap().len(), 1);
}

#[test]
fn relationship_adds_require_relationship_refs_without_reclassifying_resource_updates() {
    let relationship_add = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "ref": {"type": "articles", "id": "1", "relationship": "tags"},
            "data": [{"type": "tags", "id": "tag-1"}]
        }]
    }))
    .unwrap();
    assert!(matches!(
        relationship_add[0].operation,
        PlannedOperation::AddRelationshipMembers { .. }
    ));

    let resource_update = plan(json!({
        "atomic:operations": [{
            "op": "update",
            "ref": {"type": "articles", "id": "1"},
            "data": {"type": "articles", "id": "1", "attributes": {"title": "Updated"}}
        }]
    }))
    .unwrap();
    assert!(matches!(
        resource_update[0].operation,
        PlannedOperation::UpdateResource { .. }
    ));

    let missing_relationship = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "ref": {"type": "articles", "id": "1"},
            "data": [{"type": "tags", "id": "tag-1"}]
        }]
    }));
    assert!(matches!(
        missing_relationship,
        Err(AtomicOperationsError::InvalidOperation {
            index: 0,
            pointer,
            ..
        }) if pointer == "/atomic:operations/0/ref"
    ));
}

#[test]
fn rejects_malformed_operation_shapes_and_unknown_registry_fields() {
    let cases = [
        json!({"atomic:operations": [{"op": "copy"}]}),
        json!({"atomic:operations": [{"op": "add"}]}),
        json!({"atomic:operations": [{"op": "update", "data": {"type": "authors"}}]}),
        json!({"atomic:operations": [{"op": "remove"}]}),
        json!({"atomic:operations": [{
            "op": "add",
            "ref": {"type": "articles", "id": "1"},
            "data": [{"type": "tags", "id": "2"}]
        }]}),
        json!({"atomic:operations": [{
            "op": "remove",
            "ref": {"type": "articles", "id": "1"},
            "data": null
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "href": "/authors",
            "ref": {"type": "authors", "id": "1"},
            "data": {"type": "authors"}
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "attributes": {"secret": "hidden"}}
        }]}),
        json!({"atomic:operations": [{
            "op": "add",
            "data": {"type": "articles", "relationships": {
                "author": {"data": {"type": "tags", "id": "2"}}
            }}
        }]}),
    ];
    for value in cases {
        assert!(plan(value).is_err());
    }
}

#[test]
fn validates_operation_specific_request_data_shapes() {
    let valid_operations = [
        json!({"op": "add", "data": {"type": "authors"}}),
        json!({
            "op": "add",
            "ref": {"type": "articles", "id": "1", "relationship": "tags"},
            "data": [{"type": "tags", "id": "2"}]
        }),
        json!({
            "op": "update",
            "ref": {"type": "authors", "id": "1"},
            "data": {"type": "authors", "attributes": {"name": "Ada"}}
        }),
        json!({
            "op": "update",
            "ref": {"type": "articles", "id": "1", "relationship": "author"},
            "data": null
        }),
        json!({"op": "remove", "ref": {"type": "authors", "id": "1"}}),
        json!({
            "op": "remove",
            "ref": {"type": "articles", "id": "1", "relationship": "tags"},
            "data": [{"type": "tags", "id": "2"}]
        }),
    ];
    for operation in valid_operations {
        assert!(
            plan(json!({"atomic:operations": [operation]})).is_ok(),
            "expected valid operation: {operation}"
        );
    }

    let invalid_operations = [
        (
            json!({
                "op": "add",
                "ref": {"type": "articles", "id": "1", "relationship": "tags"},
                "data": {"type": "tags", "id": "2"}
            }),
            "/atomic:operations/0",
        ),
        (
            json!({
                "op": "update",
                "ref": {"type": "articles", "id": "1", "relationship": "author"}
            }),
            "/atomic:operations/0",
        ),
        (
            json!({
                "op": "remove",
                "ref": {"type": "articles", "id": "1", "relationship": "tags"},
                "data": null
            }),
            "/atomic:operations/0",
        ),
        (
            json!({
                "op": "add",
                "ref": {"type": "authors", "id": "1"},
                "data": {"type": "authors"}
            }),
            "/atomic:operations/0/ref",
        ),
        (
            json!({
                "op": "remove",
                "ref": {"type": "authors", "id": "1"},
                "data": null
            }),
            "/atomic:operations/0",
        ),
    ];
    for (operation, expected_pointer) in invalid_operations {
        assert!(matches!(
            plan(json!({"atomic:operations": [operation]})),
            Err(AtomicOperationsError::InvalidOperation { index: 0, pointer, .. })
                if pointer == expected_pointer
        ));
    }
}

#[test]
fn unknown_resource_attributes_point_to_the_nested_data_member() {
    let valid = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "attributes": {"name": "Ada"}}
        }]
    }))
    .unwrap();
    let PlannedOperation::AddResource { changeset, .. } = &valid[0].operation else {
        panic!("expected an add-resource operation");
    };
    assert_eq!(
        changeset.attributes.as_ref().unwrap().get("name"),
        Some(&json!("Ada"))
    );

    let error = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "attributes": {"unknown": "value"}}
        }]
    }))
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicOperationsError::InvalidOperation {
            index: 0,
            pointer,
            ..
        } if pointer == "/atomic:operations/0/data/attributes/unknown"
    ));
}

#[test]
fn unknown_resource_relationships_point_to_escaped_nested_members() {
    let valid = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {
                "type": "articles",
                "relationships": {
                    "author": {"data": {"type": "authors", "id": "author-1"}}
                }
            }
        }]
    }))
    .unwrap();
    let PlannedOperation::AddResource { changeset, .. } = &valid[0].operation else {
        panic!("expected an add-resource operation");
    };
    assert!(
        changeset
            .relationships
            .as_ref()
            .unwrap()
            .contains_key("author_id")
    );

    let error = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {
                "type": "articles",
                "relationships": {
                    "secret/owner~": {"data": {"type": "authors", "id": "author-1"}}
                }
            }
        }]
    }))
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicOperationsError::InvalidOperation {
            index: 0,
            pointer,
            ..
        } if pointer == "/atomic:operations/0/data/relationships/secret~1owner~0"
    ));
}

#[test]
fn atomic_references_require_type_and_exactly_one_identity() {
    let resource_id_reference = plan(json!({
        "atomic:operations": [{
            "op": "remove",
            "ref": {"type": "authors", "id": "author-1"}
        }]
    }))
    .unwrap();
    assert_eq!(resource_id_reference.len(), 1);

    let local_id_reference = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author-local"}},
            {"op": "remove", "ref": {"type": "authors", "lid": "author-local"}}
        ]
    }))
    .unwrap();
    assert_eq!(local_id_reference.len(), 2);

    for reference in [
        json!({"type": "authors"}),
        json!({"type": "authors", "id": "author-1", "lid": "author-local"}),
    ] {
        let request = json!({
            "atomic:operations": [
                {"op": "add", "data": {"type": "authors", "lid": "author-local"}},
                {"op": "remove", "ref": reference}
            ]
        });
        assert!(matches!(
            plan(request),
            Err(AtomicOperationsError::InvalidOperation { index: 1, .. })
        ));
    }
}

#[test]
fn rejects_forward_duplicate_and_mismatched_local_id_references() {
    let forward = plan(json!({
        "atomic:operations": [
            {
                "op": "add",
                "data": {"type": "articles", "relationships": {
                    "author": {"data": {"type": "authors", "lid": "future"}}
                }}
            },
            {"op": "add", "data": {"type": "authors", "lid": "future"}}
        ]
    }));
    assert!(matches!(
        forward,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));

    let duplicate = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "same"}},
            {"op": "add", "data": {"type": "authors", "lid": "same"}}
        ]
    }));
    assert!(matches!(
        duplicate,
        Err(AtomicOperationsError::InvalidOperation { index: 1, .. })
    ));

    let mismatch = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author"}},
            {
                "op": "add",
                "data": {"type": "articles", "relationships": {
                    "author": {"data": {"type": "authors", "lid": "not-author"}}
                }}
            }
        ]
    }));
    assert!(mismatch.is_err());
}

#[test]
fn local_ids_must_come_from_a_preceding_resource_add() {
    let forward_reference = plan(json!({
        "atomic:operations": [
            {"op": "remove", "ref": {"type": "authors", "lid": "future"}},
            {"op": "add", "data": {"type": "authors", "lid": "future"}}
        ]
    }));
    assert!(matches!(
        forward_reference,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));

    let unknown_reference = plan(json!({
        "atomic:operations": [{
            "op": "remove",
            "ref": {"type": "authors", "lid": "unknown"}
        }]
    }));
    assert!(matches!(
        unknown_reference,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));

    let update_cannot_declare_a_local_id = plan(json!({
        "atomic:operations": [
            {
                "op": "update",
                "ref": {"type": "authors", "id": "author-1"},
                "data": {
                    "type": "authors",
                    "lid": "from-update",
                    "attributes": {"name": "Updated"}
                }
            },
            {"op": "remove", "ref": {"type": "authors", "lid": "from-update"}}
        ]
    }));
    assert!(matches!(
        update_cannot_declare_a_local_id,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));
}

#[test]
fn rejects_local_ids_referenced_by_their_own_add_operation() {
    let categories = ResourceDefinition::new("categories", "category_id").relationship(
        "parent",
        "parent_id",
        "categories",
    );
    let registry = ResourceRegistry::new([categories]).unwrap();
    let request = document(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {
                "type": "categories",
                "lid": "self",
                "relationships": {
                    "parent": {"data": {"type": "categories", "lid": "self"}}
                }
            }
        }]
    }));
    assert!(matches!(
        plan_atomic_operations(&registry, &request),
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));
}

#[test]
fn resolves_resource_and_collection_href_targets_before_execution() {
    let registry = registry();
    let planned = plan_atomic_operations_with_href_resolver(
        &registry,
        &document(json!({
            "atomic:operations": [
                {"op": "add", "href": "/articles", "data": {"type": "articles", "attributes": {"title": "Created"}}},
                {"op": "update", "href": "/articles/7", "data": {"type": "articles", "attributes": {"title": "Updated"}}},
                {"op": "remove", "href": "/articles/7"}
            ]
        })),
        &TestHrefResolver,
    )
    .unwrap();
    assert!(matches!(
        &planned[0].operation,
        PlannedOperation::AddResource { href: None, .. }
    ));
    assert!(matches!(
        &planned[1].operation,
        PlannedOperation::UpdateResource {
            target: seamark::atomic::AtomicTarget::Reference(reference),
            ..
        } if reference.type_name == "articles" && reference.id.as_deref() == Some("7")
    ));
    assert!(matches!(
        &planned[2].operation,
        PlannedOperation::RemoveResource {
            target: seamark::atomic::AtomicTarget::Reference(reference),
        } if reference.type_name == "articles" && reference.id.as_deref() == Some("7")
    ));

    let mismatch = plan_atomic_operations_with_href_resolver(
        &registry,
        &document(json!({
            "atomic:operations": [
                {"op": "add", "href": "/articles", "data": {"type": "authors", "attributes": {"name": "Ada"}}}
            ]
        })),
        &TestHrefResolver,
    );
    assert!(matches!(
        mismatch,
        Err(AtomicOperationsError::InvalidOperation { index: 0, .. })
    ));
}

#[test]
fn rejects_unassigned_local_ids() {
    let local_ids = LocalIdMap::default();
    let author = ResourceIdentifier {
        type_name: "authors".to_owned(),
        lid: Some("local-1".to_owned()),
        ..ResourceIdentifier::default()
    };
    assert!(local_ids.resolve(&author).is_err());
}

struct TestGuard {
    authorized: bool,
    maximum_operations: usize,
}

#[async_trait]
impl AtomicOperationsGuard for TestGuard {
    async fn authorize(
        &self,
        _headers: &HeaderMap,
        _operations: &[PlannedAtomicOperation],
    ) -> bool {
        self.authorized
    }

    fn validate_limits(&self, operations: &[PlannedAtomicOperation]) -> Result<(), String> {
        if operations.len() > self.maximum_operations {
            return Err("operation count limit exceeded".to_owned());
        }
        Ok(())
    }
}

struct LogHandler {
    fail_update: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl AtomicOperationHandler for LogHandler {
    async fn execute_operation(
        &self,
        transaction: &sea_orm::DatabaseTransaction,
        operation: &PlannedOperation,
        local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match operation {
            PlannedOperation::AddResource { data, .. } => {
                let related_id = data
                    .relationships
                    .as_ref()
                    .and_then(|relationships| relationships.get("author"))
                    .and_then(|relationship| relationship.data.as_ref())
                    .and_then(|relationship_data| match relationship_data {
                        RelationshipData::One(identifier) => Some(identifier),
                        _ => None,
                    })
                    .map(|identifier| local_ids.resolve(identifier))
                    .transpose()?
                    .and_then(|identifier| identifier.id);
                let (event, created_resource) = match data.type_name.as_str() {
                    "authors" => (
                        "author",
                        data.lid.as_ref().map(|_| ResourceIdentifier {
                            type_name: "authors".to_owned(),
                            id: Some("101".to_owned()),
                            ..ResourceIdentifier::default()
                        }),
                    ),
                    "articles" => (
                        "article",
                        data.lid.as_ref().map(|_| ResourceIdentifier {
                            type_name: "articles".to_owned(),
                            id: Some("201".to_owned()),
                            ..ResourceIdentifier::default()
                        }),
                    ),
                    _ => return Err("unexpected resource type".to_owned()),
                };
                transaction
                    .execute(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "INSERT INTO seamark_atomic_log (event, related_id) VALUES ($1, $2)",
                        [event.into(), related_id.into()],
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(AtomicOperationOutcome {
                    result: AtomicResult {
                        data: Some(
                            json!({"type": data.type_name, "id": created_resource.as_ref().and_then(|id| id.id.clone()).unwrap_or_else(|| "202".to_owned())}),
                        ),
                        meta: None,
                    },
                    created_resource,
                })
            }
            PlannedOperation::UpdateResource { .. } => {
                transaction
                    .execute_unprepared("INSERT INTO seamark_atomic_log (event) VALUES ('update')")
                    .await
                    .map_err(|error| error.to_string())?;
                if self.fail_update {
                    Err("injected update failure".to_owned())
                } else {
                    Ok(AtomicOperationOutcome::default())
                }
            }
            _ => Ok(AtomicOperationOutcome::default()),
        }
    }
}

struct MismatchedLocalIdResultHandler;

#[async_trait]
impl AtomicOperationHandler for MismatchedLocalIdResultHandler {
    async fn execute_operation(
        &self,
        transaction: &sea_orm::DatabaseTransaction,
        _operation: &PlannedOperation,
        _local_ids: &LocalIdMap,
    ) -> Result<AtomicOperationOutcome, String> {
        transaction
            .execute_unprepared(
                "INSERT INTO seamark_atomic_mismatched_local_id_log (event) VALUES ('created')",
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(AtomicOperationOutcome {
            result: AtomicResult {
                data: Some(json!({"type": "authors", "id": "returned-id"})),
                meta: None,
            },
            created_resource: Some(ResourceIdentifier {
                type_name: "authors".to_owned(),
                id: Some("mapped-id".to_owned()),
                ..ResourceIdentifier::default()
            }),
        })
    }
}

async fn database() -> DatabaseConnection {
    let url = std::env::var("SEAMARK_TEST_DATABASE_URL")
        .expect("set SEAMARK_TEST_DATABASE_URL to a dedicated PostgreSQL test database");
    Database::connect(url).await.unwrap()
}

#[tokio::test]
async fn rolls_back_add_when_result_identity_disagrees_with_local_id_mapping() {
    let database = database().await;
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_atomic_mismatched_local_id_log; CREATE TABLE seamark_atomic_mismatched_local_id_log (event TEXT NOT NULL)",
        )
        .await
        .unwrap();
    let operations = plan(json!({
        "atomic:operations": [{
            "op": "add",
            "data": {"type": "authors", "lid": "author-local", "attributes": {"name": "Ada"}}
        }]
    }))
    .unwrap();
    let error = execute_atomic_operations(
        &database,
        &operations,
        &HeaderMap::new(),
        &TestGuard {
            authorized: true,
            maximum_operations: 1,
        },
        &MismatchedLocalIdResultHandler,
    )
    .await;
    assert!(matches!(
        error,
        Err(AtomicExecutionError::InvalidResult { index: 0, .. })
    ));
    let count = database
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            "SELECT COUNT(*) AS count FROM seamark_atomic_mismatched_local_id_log",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "count")
        .unwrap();
    assert_eq!(count, 0);
    database.close().await.unwrap();
}

#[tokio::test]
async fn executes_operations_in_order_maps_local_ids_and_rolls_back_failures() {
    let database = database().await;
    database
        .execute_unprepared(
            "DROP TABLE IF EXISTS seamark_atomic_log; CREATE TABLE seamark_atomic_log (id BIGSERIAL PRIMARY KEY, event TEXT NOT NULL, related_id TEXT)",
        )
        .await
        .unwrap();
    let operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "lid": "author-local", "attributes": {"name": "Ada"}}},
            {
                "op": "add",
                "data": {
                    "type": "articles",
                    "lid": "article-local",
                    "relationships": {"author": {"data": {"type": "authors", "lid": "author-local"}}}
                }
            }
        ]
    }))
    .unwrap();
    let handler = LogHandler {
        fail_update: false,
        calls: AtomicUsize::new(0),
    };
    let guard = TestGuard {
        authorized: true,
        maximum_operations: 5,
    };
    let headers = HeaderMap::new();
    let results = execute_atomic_operations(&database, &operations, &headers, &guard, &handler)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
    let rows = database
        .query_all(Statement::from_string(
            DbBackend::Postgres,
            "SELECT event, related_id FROM seamark_atomic_log ORDER BY id",
        ))
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<String>("", "event").unwrap(), "author");
    assert_eq!(rows[1].try_get::<String>("", "event").unwrap(), "article");
    assert_eq!(
        rows[1]
            .try_get::<Option<String>>("", "related_id")
            .unwrap()
            .as_deref(),
        Some("101")
    );

    database
        .execute_unprepared("TRUNCATE seamark_atomic_log")
        .await
        .unwrap();
    let failing_operations = plan(json!({
        "atomic:operations": [
            {"op": "add", "data": {"type": "authors", "attributes": {"name": "First"}}},
            {"op": "update", "ref": {"type": "authors", "id": "10"}, "data": {"type": "authors", "id": "10", "attributes": {"name": "Second"}}}
        ]
    }))
    .unwrap();
    let failing_handler = LogHandler {
        fail_update: true,
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        execute_atomic_operations(
            &database,
            &failing_operations,
            &headers,
            &guard,
            &failing_handler
        )
        .await,
        Err(AtomicExecutionError::Operation { index: 1, .. })
    ));
    let count = database
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            "SELECT COUNT(*) AS count FROM seamark_atomic_log",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "count")
        .unwrap();
    assert_eq!(count, 0);

    let denied_guard = TestGuard {
        authorized: false,
        maximum_operations: 5,
    };
    let untouched_handler = LogHandler {
        fail_update: false,
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        execute_atomic_operations(
            &database,
            &failing_operations,
            &headers,
            &denied_guard,
            &untouched_handler
        )
        .await,
        Err(AtomicExecutionError::NotAuthorized)
    ));
    assert_eq!(untouched_handler.calls.load(Ordering::SeqCst), 0);
    let limited_guard = TestGuard {
        authorized: true,
        maximum_operations: 1,
    };
    assert!(matches!(
        execute_atomic_operations(
            &database,
            &failing_operations,
            &headers,
            &limited_guard,
            &untouched_handler
        )
        .await,
        Err(AtomicExecutionError::LimitExceeded(_))
    ));
    assert_eq!(untouched_handler.calls.load(Ordering::SeqCst), 0);

    database
        .execute_unprepared("DROP TABLE seamark_atomic_log")
        .await
        .unwrap();
}
