#![allow(missing_docs)]

use std::collections::BTreeMap;

use seamark::query::{
    FilterError, FilterExpression, FilterValue, IncludeNode, Page, PaginationConfig, PlannedField,
    ReadPlanError, ReadQuery, SortDirection, SortField, parse_filter, plan_filters, plan_read,
    plan_resource_read,
};
use seamark::registry::{ResourceDefinition, ResourceRegistry};

fn registry() -> ResourceRegistry {
    let ports = ResourceDefinition::new("ports", "port_id")
        .attribute("name", "title", true, true)
        .attribute("capacity", "berth_count", false, false)
        .attribute("depth", "depth_m", false, true)
        .relationship("owner", "owner_id", "people");
    let people = ResourceDefinition::new("people", "person_id")
        .attribute("name", "display_name", true, true)
        .relationship("organization", "organization_id", "organizations");
    let organizations = ResourceDefinition::new("organizations", "organization_id").attribute(
        "name",
        "legal_name",
        false,
        true,
    );
    ResourceRegistry::new([ports, people, organizations]).unwrap()
}

fn equals(model_field: &str, value: FilterValue) -> FilterExpression {
    FilterExpression::Equals {
        model_field: model_field.to_owned(),
        value,
    }
}

#[test]
fn parses_string_equality_and_resolves_internal_field_mapping() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(name,'harbor')"),
        Ok(equals("title", FilterValue::String("harbor".to_owned())))
    );
}

#[test]
fn parses_escaped_apostrophes_and_unicode_in_strings() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(name,'O''Brien 🚢')"),
        Ok(equals(
            "title",
            FilterValue::String("O'Brien 🚢".to_owned())
        ))
    );
}

#[test]
fn parses_null_equality() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(name,null)"),
        Ok(equals("title", FilterValue::Null))
    );
}

#[test]
fn parses_nested_boolean_expressions() {
    assert_eq!(
        parse_filter(
            &registry(),
            "ports",
            "and(equals(name,'A'),or(equals(name,'B'),not(equals(name,null))))"
        ),
        Ok(FilterExpression::And(vec![
            equals("title", FilterValue::String("A".to_owned())),
            FilterExpression::Or(vec![
                equals("title", FilterValue::String("B".to_owned())),
                FilterExpression::Not(Box::new(equals("title", FilterValue::Null))),
            ]),
        ]))
    );
}

#[test]
fn accepts_whitespace_around_expression_syntax() {
    assert_eq!(
        parse_filter(
            &registry(),
            "ports",
            " and ( equals ( name , 'A' ) , equals(name, null) ) "
        ),
        Ok(FilterExpression::And(vec![
            equals("title", FilterValue::String("A".to_owned())),
            equals("title", FilterValue::Null),
        ]))
    );
}

#[test]
fn enforces_boolean_operator_arities() {
    for input in [
        "and()",
        "and(equals(name,'A'))",
        "or()",
        "or(equals(name,null))",
    ] {
        assert!(matches!(
            parse_filter(&registry(), "ports", input),
            Err(FilterError::Malformed { .. })
        ));
    }
    for input in ["not()", "not(equals(name,null),equals(name,'A'))"] {
        assert!(matches!(
            parse_filter(&registry(), "ports", input),
            Err(FilterError::Malformed { .. })
        ));
    }
}

#[test]
fn rejects_malformed_literals_parentheses_and_trailing_input() {
    for input in [
        "equals(name,'unterminated)",
        "equals(name,unquoted)",
        "equals(name,'A'",
        "equals(name,'A'))",
        "equals(name,'A') trailing",
        "equals(,'A')",
    ] {
        assert!(matches!(
            parse_filter(&registry(), "ports", input),
            Err(FilterError::Malformed { .. })
        ));
    }
}

#[test]
fn repeated_filters_combine_with_or_but_one_is_unchanged() {
    let registry = registry();
    let one = equals("title", FilterValue::String("A".to_owned()));
    assert_eq!(
        plan_filters(&registry, "ports", &["equals(name,'A')"]),
        Ok(Some(one.clone()))
    );
    assert_eq!(
        plan_filters(
            &registry,
            "ports",
            &[
                "equals(name,'A')",
                "equals(name,null)",
                "not(equals(name,'B'))"
            ]
        ),
        Ok(Some(FilterExpression::Or(vec![
            one,
            equals("title", FilterValue::Null),
            FilterExpression::Not(Box::new(equals(
                "title",
                FilterValue::String("B".to_owned())
            ))),
        ])))
    );
    assert_eq!(plan_filters(&registry, "ports", &[]), Ok(None));
}

#[test]
fn rejects_attributes_that_are_not_opted_in_for_filtering() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(capacity,null)"),
        Err(FilterError::AttributeNotFilterable {
            resource_type: "ports".to_owned(),
            field: "capacity".to_owned(),
        })
    );
}

#[test]
fn rejects_unknown_attributes_and_resource_types_explicitly() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(secret,'x')"),
        Err(FilterError::UnknownAttribute {
            resource_type: "ports".to_owned(),
            field: "secret".to_owned(),
        })
    );
    assert_eq!(
        parse_filter(&registry(), "ships", "equals(name,'x')"),
        Err(FilterError::UnknownResourceType("ships".to_owned()))
    );
}

#[test]
fn rejects_relationship_paths_and_unsupported_operators() {
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(owner.name,'x')"),
        Err(FilterError::UnsupportedPath("owner.name".to_owned()))
    );
    assert_eq!(
        parse_filter(&registry(), "ports", "equals(owner,'x')"),
        Err(FilterError::UnsupportedPath("owner".to_owned()))
    );
    assert_eq!(
        parse_filter(&registry(), "ports", "contains(name,'x')"),
        Err(FilterError::UnsupportedOperator("contains".to_owned()))
    );
}

fn pagination_config() -> PaginationConfig {
    PaginationConfig::new(1, 20, Some(100), Some(1_000)).unwrap()
}

#[test]
fn read_plan_maps_ordered_sort_fields_and_directions() {
    let query = ReadQuery {
        sort: Some("-depth,name".to_owned()),
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &pagination_config())
            .unwrap()
            .sort,
        vec![
            SortField {
                public_name: "depth".to_owned(),
                model_field: "depth_m".to_owned(),
                direction: SortDirection::Descending,
            },
            SortField {
                public_name: "name".to_owned(),
                model_field: "title".to_owned(),
                direction: SortDirection::Ascending,
            },
        ]
    );
}

#[test]
fn read_plan_rejects_unopted_unknown_duplicate_and_malformed_sort_fields() {
    let cases = [
        (
            "capacity",
            ReadPlanError::AttributeNotSortable {
                resource_type: "ports".to_owned(),
                field: "capacity".to_owned(),
            },
        ),
        (
            "secret",
            ReadPlanError::UnknownSortAttribute {
                resource_type: "ports".to_owned(),
                field: "secret".to_owned(),
            },
        ),
        (
            "name,-name",
            ReadPlanError::DuplicateSortField("name".to_owned()),
        ),
        (
            "name,,capacity",
            ReadPlanError::InvalidSort("name,,capacity".to_owned()),
        ),
        ("+name", ReadPlanError::InvalidSort("+name".to_owned())),
    ];
    for (sort, expected) in cases {
        let query = ReadQuery {
            sort: Some(sort.to_owned()),
            ..ReadQuery::default()
        };
        assert_eq!(
            plan_read(&registry(), "ports", &query, &pagination_config()),
            Err(expected)
        );
    }
}

#[test]
fn pagination_uses_explicit_defaults_and_maps_overrides_to_offset_and_limit() {
    let default_plan = plan_read(
        &registry(),
        "ports",
        &ReadQuery::default(),
        &pagination_config(),
    )
    .unwrap();
    assert_eq!(
        default_plan.page,
        Page {
            number: 1,
            size: 20,
            offset: 0,
            limit: 20,
        }
    );

    let query = ReadQuery {
        page_number: Some("3".to_owned()),
        page_size: Some("7".to_owned()),
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &pagination_config())
            .unwrap()
            .page,
        Page {
            number: 3,
            size: 7,
            offset: 14,
            limit: 7,
        }
    );
}

#[test]
fn pagination_rejects_invalid_configuration_and_client_boundaries() {
    for (number, size, maximum_size, maximum_offset) in [
        (0, 20, None, None),
        (1, 0, None, None),
        (1, 20, Some(19), None),
        (3, 20, None, Some(39)),
    ] {
        assert!(matches!(
            PaginationConfig::new(number, size, maximum_size, maximum_offset),
            Err(ReadPlanError::InvalidPaginationConfig(_))
        ));
    }

    let config = PaginationConfig::new(1, 20, Some(50), Some(100)).unwrap();
    for (parameter, value) in [
        ("page[number]", "0"),
        ("page[number]", "-1"),
        ("page[number]", "1.5"),
        ("page[size]", ""),
        ("page[size]", "18446744073709551616"),
    ] {
        let query = if parameter == "page[number]" {
            ReadQuery {
                page_number: Some(value.to_owned()),
                ..ReadQuery::default()
            }
        } else {
            ReadQuery {
                page_size: Some(value.to_owned()),
                ..ReadQuery::default()
            }
        };
        assert!(matches!(
            plan_read(&registry(), "ports", &query, &config),
            Err(ReadPlanError::InvalidPageParameter { .. })
        ));
    }
    let query = ReadQuery {
        page_size: Some("51".to_owned()),
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &config),
        Err(ReadPlanError::PageSizeExceedsMaximum {
            requested: 51,
            maximum: 50,
        })
    );
    let query = ReadQuery {
        page_number: Some("7".to_owned()),
        page_size: Some("20".to_owned()),
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &config),
        Err(ReadPlanError::PageOffsetExceedsMaximum {
            requested: 120,
            maximum: 100,
        })
    );
}

#[test]
fn read_plan_maps_sparse_fieldsets_for_attributes_and_relationships() {
    let query = ReadQuery {
        fieldsets: BTreeMap::from([
            ("people".to_owned(), "name".to_owned()),
            ("ports".to_owned(), "name,owner".to_owned()),
        ]),
        ..ReadQuery::default()
    };
    let plan = plan_read(&registry(), "ports", &query, &pagination_config()).unwrap();
    assert_eq!(
        plan.fieldsets,
        BTreeMap::from([
            (
                "people".to_owned(),
                vec![PlannedField::Attribute {
                    public_name: "name".to_owned(),
                    model_field: "display_name".to_owned(),
                }],
            ),
            (
                "ports".to_owned(),
                vec![
                    PlannedField::Attribute {
                        public_name: "name".to_owned(),
                        model_field: "title".to_owned(),
                    },
                    PlannedField::Relationship {
                        public_name: "owner".to_owned(),
                        model_field: "owner_id".to_owned(),
                        target_type: "people".to_owned(),
                    },
                ],
            ),
        ])
    );
}

#[test]
fn fieldsets_allow_empty_projection_and_reject_unknown_or_duplicate_fields() {
    let empty = ReadQuery {
        fieldsets: BTreeMap::from([("ports".to_owned(), String::new())]),
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &empty, &pagination_config())
            .unwrap()
            .fieldsets,
        BTreeMap::from([("ports".to_owned(), Vec::new())])
    );

    for (value, expected) in [
        (
            "unknown",
            ReadPlanError::UnknownFieldsetField {
                resource_type: "ports".to_owned(),
                field: "unknown".to_owned(),
            },
        ),
        (
            "name,name",
            ReadPlanError::InvalidFieldset {
                resource_type: "ports".to_owned(),
                value: "name,name".to_owned(),
            },
        ),
        (
            "name,",
            ReadPlanError::InvalidFieldset {
                resource_type: "ports".to_owned(),
                value: "name,".to_owned(),
            },
        ),
    ] {
        let query = ReadQuery {
            fieldsets: BTreeMap::from([("ports".to_owned(), value.to_owned())]),
            ..ReadQuery::default()
        };
        assert_eq!(
            plan_read(&registry(), "ports", &query, &pagination_config()),
            Err(expected)
        );
    }
}

#[test]
fn includes_validate_and_merge_nested_relationship_paths_deterministically() {
    let query = ReadQuery {
        includes: vec![
            "owner".to_owned(),
            "owner.organization".to_owned(),
            "owner.organization".to_owned(),
        ],
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &pagination_config())
            .unwrap()
            .includes,
        vec![IncludeNode {
            public_name: "owner".to_owned(),
            model_field: "owner_id".to_owned(),
            target_type: "people".to_owned(),
            children: vec![IncludeNode {
                public_name: "organization".to_owned(),
                model_field: "organization_id".to_owned(),
                target_type: "organizations".to_owned(),
                children: vec![],
            }],
        }]
    );

    let empty = ReadQuery {
        includes: vec![String::new()],
        ..ReadQuery::default()
    };
    assert!(
        plan_read(&registry(), "ports", &empty, &pagination_config())
            .unwrap()
            .includes
            .is_empty()
    );
}

#[test]
fn include_paths_reject_unknown_relationships_and_empty_segments() {
    for (include, expected) in [
        (
            "missing",
            ReadPlanError::UnknownRelationship {
                resource_type: "ports".to_owned(),
                relationship: "missing".to_owned(),
            },
        ),
        (
            "owner.missing",
            ReadPlanError::UnknownRelationship {
                resource_type: "people".to_owned(),
                relationship: "missing".to_owned(),
            },
        ),
        (
            "owner..organization",
            ReadPlanError::InvalidIncludePath("owner..organization".to_owned()),
        ),
        (
            " owner",
            ReadPlanError::InvalidIncludePath(" owner".to_owned()),
        ),
    ] {
        let query = ReadQuery {
            includes: vec![include.to_owned()],
            ..ReadQuery::default()
        };
        assert_eq!(
            plan_read(&registry(), "ports", &query, &pagination_config()),
            Err(expected)
        );
    }
}

#[test]
fn unknown_query_parameters_are_rejected_sorted_and_deduplicated() {
    let query = ReadQuery {
        unsupported_parameters: vec![
            "cursor".to_owned(),
            "z".to_owned(),
            "cursor".to_owned(),
            "a".to_owned(),
        ],
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_read(&registry(), "ports", &query, &pagination_config()),
        Err(ReadPlanError::UnsupportedQueryParameters(vec![
            "a".to_owned(),
            "cursor".to_owned(),
            "z".to_owned(),
        ]))
    );
}

#[test]
fn single_resource_read_plans_includes_and_fieldsets() {
    let query = ReadQuery {
        fieldsets: BTreeMap::from([
            ("ports".to_owned(), "name,owner".to_owned()),
            ("people".to_owned(), "name".to_owned()),
        ]),
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    };
    let plan = plan_resource_read(&registry(), "ports", &query, &pagination_config()).unwrap();

    assert_eq!(plan.resource_type, "ports");
    assert!(plan.filter.is_none());
    assert!(plan.sort.is_empty());
    assert_eq!(
        plan.page,
        Page {
            number: 1,
            size: 1,
            offset: 0,
            limit: 1,
        }
    );
    assert_eq!(plan.fieldsets["ports"].len(), 2);
    assert_eq!(plan.fieldsets["people"].len(), 1);
    assert_eq!(
        plan.includes,
        vec![IncludeNode {
            public_name: "owner".to_owned(),
            model_field: "owner_id".to_owned(),
            target_type: "people".to_owned(),
            children: Vec::new(),
        }]
    );
}

#[test]
fn single_resource_read_rejects_collection_and_unknown_parameters_deterministically() {
    let query = ReadQuery {
        filters: vec!["equals(name,'A')".to_owned()],
        sort: Some("name".to_owned()),
        page_number: Some("2".to_owned()),
        page_size: Some("5".to_owned()),
        unsupported_parameters: vec!["cursor".to_owned()],
        ..ReadQuery::default()
    };
    assert_eq!(
        plan_resource_read(&registry(), "ports", &query, &pagination_config()),
        Err(ReadPlanError::UnsupportedQueryParameters(vec![
            "cursor".to_owned(),
            "filter".to_owned(),
            "page[number]".to_owned(),
            "page[size]".to_owned(),
            "sort".to_owned(),
        ]))
    );
}

#[test]
fn one_read_plan_integrates_filters_sort_pagination_fieldsets_and_includes() {
    let query = ReadQuery {
        filters: vec!["equals(name,'A')".to_owned(), "equals(name,'B')".to_owned()],
        sort: Some("-name".to_owned()),
        page_number: Some("2".to_owned()),
        page_size: Some("10".to_owned()),
        fieldsets: BTreeMap::from([("ports".to_owned(), "name,owner".to_owned())]),
        includes: vec!["owner".to_owned()],
        ..ReadQuery::default()
    };
    let plan = plan_read(&registry(), "ports", &query, &pagination_config()).unwrap();
    assert_eq!(
        plan.filter,
        Some(FilterExpression::Or(vec![
            equals("title", FilterValue::String("A".to_owned())),
            equals("title", FilterValue::String("B".to_owned())),
        ]))
    );
    assert_eq!(plan.sort[0].direction, SortDirection::Descending);
    assert_eq!(
        plan.page,
        Page {
            number: 2,
            size: 10,
            offset: 10,
            limit: 10,
        }
    );
    assert_eq!(plan.fieldsets["ports"].len(), 2);
    assert_eq!(plan.includes[0].public_name, "owner");
}
