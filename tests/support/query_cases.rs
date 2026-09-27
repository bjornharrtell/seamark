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
