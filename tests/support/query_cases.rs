use std::collections::BTreeMap;

use seamark::query::ReadQuery;
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

pub struct PortFixture {
    pub id: i32,
    pub name: &'static str,
    pub capacity: Option<i32>,
    pub depth: i32,
    pub active: bool,
    pub owner_id: i32,
}

pub const PORTS: [PortFixture; 3] = [
    PortFixture {
        id: 1,
        name: "Alpha",
        capacity: Some(4),
        depth: 2,
        active: true,
        owner_id: 11,
    },
    PortFixture {
        id: 2,
        name: "Beta",
        capacity: Some(8),
        depth: 9,
        active: false,
        owner_id: 12,
    },
    PortFixture {
        id: 3,
        name: "Gamma",
        capacity: None,
        depth: 6,
        active: true,
        owner_id: 11,
    },
];

pub const FILTER_CASES: [(&str, &[&str]); 4] = [
    ("equals(capacity,'8')", &["2"]),
    ("equals(active,'true')", &["1", "3"]),
    ("equals(capacity,null)", &["3"]),
    ("equals(name,'Alpha')", &["1"]),
];

pub const FIRST_PAGE_PORT_ID: &str = "2";
pub const FIRST_PAGE_PORT_NAME: &str = "Beta";
pub const FIRST_PAGE_OWNER_ID: &str = "12";
pub const FIRST_PAGE_OWNER_NAME: &str = "Niko";
pub const SECOND_PAGE_PORT_ID: &str = "1";

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
