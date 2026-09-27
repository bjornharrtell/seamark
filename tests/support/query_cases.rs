use std::collections::BTreeMap;

use seamark::query::ReadQuery;
use seamark::seaorm::SeaOrmReadResult;
use serde_json::{Value, json};

pub struct PersonFixture {
    pub id: i32,
    pub name: &'static str,
    pub note: &'static str,
}

pub const PEOPLE: [PersonFixture; 2] = [
    PersonFixture {
        id: 11,
        name: "Mara",
        note: "secret A",
    },
    PersonFixture {
        id: 12,
        name: "Niko",
        note: "secret B",
    },
];

pub fn expected_person_attributes(id: &str) -> BTreeMap<String, Value> {
    let person = PEOPLE
        .iter()
        .find(|person| person.id.to_string() == id)
        .expect("included person ID must match a shared fixture");
    BTreeMap::from([
        ("display_name".to_owned(), json!(person.name)),
        ("private_note".to_owned(), json!(person.note)),
    ])
}

pub struct PortFixture {
    pub id: i32,
    pub name: &'static str,
    pub capacity: Option<i32>,
    pub depth: i32,
    pub active: bool,
    pub owner_id: Option<i32>,
}

pub const PORTS: [PortFixture; 3] = [
    PortFixture {
        id: 1,
        name: "Alpha",
        capacity: Some(4),
        depth: 2,
        active: true,
        owner_id: Some(11),
    },
    PortFixture {
        id: 2,
        name: "Beta",
        capacity: Some(8),
        depth: 9,
        active: false,
        owner_id: Some(12),
    },
    PortFixture {
        id: 3,
        name: "Gamma",
        capacity: None,
        depth: 6,
        active: true,
        owner_id: None,
    },
];

pub const FILTER_CASES: [(&str, &[&str]); 6] = [
    ("equals(capacity,'8')", &["2"]),
    ("equals(active,'true')", &["1", "3"]),
    ("equals(active,'false')", &["2"]),
    ("equals(capacity,null)", &["3"]),
    ("equals(name,'Alpha')", &["1"]),
    ("and(equals(name,'Beta'),not(equals(depth,'2')))", &["2"]),
];

pub const FIRST_PAGE_PORT_ID: &str = "2";
pub const FIRST_PAGE_PORT_NAME: &str = "Beta";
pub const FIRST_PAGE_OWNER_ID: &str = "12";
pub const FIRST_PAGE_OWNER_NAME: &str = "Niko";
pub const SECOND_PAGE_PORT_ID: &str = "1";
pub const NEIGHBOR_PORT_IDS: [&str; 2] = ["2", "3"];
pub const SORTED_FIRST_PAGE: [(&str, &str); 2] = [("2", "Beta"), ("3", "Gamma")];
pub const SORTED_SECOND_PAGE: [(&str, &str); 1] = [("1", "Alpha")];

pub fn single_resource_with_owner() -> ReadQuery {
    ReadQuery {
        fieldsets: BTreeMap::from([
            ("ports".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    }
}

pub fn single_resource_owner_document() -> Value {
    json!({
        "data": {
            "type": "ports",
            "id": "1",
            "attributes": {"name": "Alpha"},
            "relationships": {
                "owner": {"data": {"type": "people", "id": "11"}}
            }
        },
        "included": [{
            "type": "people",
            "id": "11",
            "attributes": {"name": "Mara"}
        }]
    })
}

pub fn single_resource_not_found_document(id: &str) -> Value {
    json!({
        "errors": [{
            "status": "404",
            "code": "resource_not_found",
            "title": "Resource not found",
            "detail": format!("No `ports` resource has id `{id}`.")
        }]
    })
}

// Application-defined self-referential to-many mapping shared by both backends.
pub fn neighbor_ids(port_id: i32) -> &'static [i32] {
    match port_id {
        1 => &[2, 3],
        2 => &[1],
        3 => &[1],
        _ => &[],
    }
}

pub fn first_page_with_owner() -> ReadQuery {
    ReadQuery {
        filters: vec![
            "equals(name,'Alpha')".to_owned(),
            "equals(name,'Beta')".to_owned(),
        ],
        sort: Some("-depth".to_owned()),
        page_number: Some("1".to_owned()),
        page_size: Some("1".to_owned()),
        fieldsets: BTreeMap::from([
            ("ports".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    }
}

pub fn second_page_without_projection() -> ReadQuery {
    ReadQuery {
        page_number: Some("2".to_owned()),
        fieldsets: BTreeMap::new(),
        includes: Vec::new(),
        ..first_page_with_owner()
    }
}

pub fn sorted_ports_page(page_number: &str) -> ReadQuery {
    ReadQuery {
        sort: Some("-depth".to_owned()),
        page_number: Some(page_number.to_owned()),
        page_size: Some("2".to_owned()),
        fieldsets: BTreeMap::from([
            ("ports".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    }
}

pub fn sorted_nullable_capacity(descending: bool) -> ReadQuery {
    ReadQuery {
        sort: Some(if descending { "-capacity" } else { "capacity" }.to_owned()),
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    }
}

pub fn multi_field_sorted_ports() -> ReadQuery {
    ReadQuery {
        sort: Some("active,-capacity".to_owned()),
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    }
}

pub fn assert_multi_field_sorted_ports(result: &SeaOrmReadResult) {
    assert_eq!(
        result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        vec!["2", "1", "3"]
    );
}

pub fn two_level_neighbors() -> ReadQuery {
    ReadQuery {
        filters: vec!["equals(name,'Beta')".to_owned()],
        includes: vec!["neighbors.neighbors".to_owned()],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    }
}

pub fn sparse_fieldset_with_owner_include() -> ReadQuery {
    ReadQuery {
        filters: vec!["equals(name,'Alpha')".to_owned()],
        fieldsets: BTreeMap::from([
            ("ports".to_owned(), "name".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        includes: vec!["owner".to_owned()],
        page_size: Some("10".to_owned()),
        ..ReadQuery::default()
    }
}

pub fn assert_sparse_fieldset_owner_include(result: &SeaOrmReadResult) {
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id, "1");
    assert_eq!(
        result.resources[0].attributes,
        BTreeMap::from([("title".to_owned(), json!("Alpha"))])
    );
    assert!(!result.resources[0].relationships.contains_key("owner_id"));
    assert_eq!(result.included.len(), 1);
    assert_eq!(result.included[0].resource_type, "people");
    assert_eq!(result.included[0].resource.id, "11");
    assert_eq!(
        result.included[0].resource.attributes,
        BTreeMap::from([("display_name".to_owned(), json!("Mara"))])
    );
}

pub fn sparse_fieldset_owner_include_document() -> Value {
    json!({
        "data": [{
            "type": "ports",
            "id": "1",
            "attributes": {"name": "Alpha"}
        }],
        "included": [{
            "type": "people",
            "id": "11",
            "attributes": {"name": "Mara"}
        }]
    })
}

pub fn assert_two_level_neighbors(result: &SeaOrmReadResult) {
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].id, "2");
    let root_neighbors = result.resources[0]
        .relationships
        .get("neighbor_ids")
        .and_then(|relationship| relationship.data.as_ref());
    let root_neighbor_ids = match root_neighbors {
        Some(seamark::document::RelationshipData::Many(identifiers)) => identifiers
            .iter()
            .map(|identifier| identifier.id.as_deref().expect("neighbor must use id"))
            .collect::<Vec<_>>(),
        other => panic!("expected root to-many linkage, got {other:?}"),
    };
    assert_eq!(root_neighbor_ids, vec!["1"]);

    assert_eq!(result.included.len(), 2);
    let included = result
        .included
        .iter()
        .map(|included| {
            assert_eq!(included.resource_type, "ports");
            let relationship = included
                .resource
                .relationships
                .get("neighbor_ids")
                .and_then(|relationship| relationship.data.as_ref());
            let neighbor_ids = match relationship {
                Some(seamark::document::RelationshipData::Many(identifiers)) => identifiers
                    .iter()
                    .map(|identifier| {
                        identifier
                            .id
                            .clone()
                            .expect("neighbor linkage must use an id")
                    })
                    .collect::<Vec<_>>(),
                other => panic!("expected included to-many linkage, got {other:?}"),
            };
            (included.resource.id.clone(), neighbor_ids)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        included,
        BTreeMap::from([
            ("1".to_owned(), vec!["2".to_owned(), "3".to_owned()]),
            ("3".to_owned(), vec!["1".to_owned()]),
        ])
    );
}

pub fn assert_single_resource_two_level_neighbors_document(document: &Value) {
    assert_eq!(
        document["data"],
        json!({
            "type": "ports",
            "id": "2",
            "attributes": {"name": "Beta"},
            "relationships": {
                "neighbors": {
                    "data": [{"type": "ports", "id": "1"}]
                }
            }
        })
    );

    let mut included = document["included"].as_array().unwrap().clone();
    included.sort_by_key(|resource| resource["id"].as_str().unwrap().to_owned());
    assert_eq!(
        included,
        vec![
            json!({
                "type": "ports",
                "id": "1",
                "attributes": {"name": "Alpha"},
                "relationships": {
                    "neighbors": {
                        "data": [
                            {"type": "ports", "id": "2"},
                            {"type": "ports", "id": "3"}
                        ]
                    }
                }
            }),
            json!({
                "type": "ports",
                "id": "3",
                "attributes": {"name": "Gamma"},
                "relationships": {
                    "neighbors": {
                        "data": [{"type": "ports", "id": "1"}]
                    }
                }
            })
        ]
    );
}

pub fn assert_sorted_ports_page(result: &SeaOrmReadResult, page_number: u8) {
    let (expected_ports, expected_owner_id, expected_owner_name) = match page_number {
        1 => (&SORTED_FIRST_PAGE[..], "12", "Niko"),
        2 => (&SORTED_SECOND_PAGE[..], "11", "Mara"),
        _ => panic!("unexpected sorted query page {page_number}"),
    };
    assert_eq!(
        result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        expected_ports.iter().map(|(id, _)| *id).collect::<Vec<_>>()
    );
    for (resource, (_, expected_name)) in result.resources.iter().zip(expected_ports) {
        assert_eq!(
            resource.attributes,
            BTreeMap::from([("title".to_owned(), json!(expected_name))])
        );
        assert!(resource.relationships.contains_key("owner_id"));
    }
    assert_eq!(
        result
            .included
            .iter()
            .map(|included| included.resource.id.as_str())
            .collect::<Vec<_>>(),
        vec![expected_owner_id]
    );
    assert_eq!(
        result.included[0].resource.attributes,
        BTreeMap::from([("display_name".to_owned(), json!(expected_owner_name))])
    );
}

pub fn assert_sorted_nullable_capacity(result: &SeaOrmReadResult, expected_ids: &[&str]) {
    assert_eq!(
        result
            .resources
            .iter()
            .map(|resource| resource.id.as_str())
            .collect::<Vec<_>>(),
        expected_ids
    );
}

pub fn first_page_document() -> Value {
    json!({
        "data": [{
            "type": "ports",
            "id": FIRST_PAGE_PORT_ID,
            "attributes": {"name": FIRST_PAGE_PORT_NAME},
            "relationships": {
                "owner": {"data": {"type": "people", "id": FIRST_PAGE_OWNER_ID}}
            }
        }],
        "included": [{
            "type": "people",
            "id": FIRST_PAGE_OWNER_ID,
            "attributes": {"name": FIRST_PAGE_OWNER_NAME}
        }]
    })
}
