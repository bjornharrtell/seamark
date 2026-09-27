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
