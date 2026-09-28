//! Adapter-independent parsing and planning for resource filters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::registry::{RegistryError, ResourceRegistry};

/// A filter expression with public attributes resolved to internal model fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilterExpression {
    /// Tests whether a mapped field equals a string or null.
    Equals {
        /// The internal model field name from the resource registry.
        model_field: String,
        /// The value compared with the field.
        value: FilterValue,
    },
    /// Requires every child expression to match.
    And(Vec<Self>),
    /// Requires at least one child expression to match.
    Or(Vec<Self>),
    /// Negates its child expression.
    Not(Box<Self>),
}

/// A supported filter comparison value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilterValue {
    /// A string supplied as a single-quoted literal.
    String(String),
    /// A null value supplied as the unquoted `null` literal.
    Null,
}

/// An error encountered while parsing or planning a filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilterError {
    /// The filter syntax is malformed at the reported byte offset.
    Malformed {
        /// Byte offset at which parsing failed.
        position: usize,
        /// A concise description of the syntax problem.
        message: &'static str,
    },
    /// The expression uses a filter operator outside the supported grammar.
    UnsupportedOperator(String),
    /// The field uses a relationship or dotted path, which is not supported.
    UnsupportedPath(String),
    /// The resource type is not registered.
    UnknownResourceType(String),
    /// The field is not a declared public attribute on the resource.
    UnknownAttribute {
        /// The public resource type.
        resource_type: String,
        /// The requested public field name.
        field: String,
    },
    /// Filtering is not explicitly enabled for the public attribute.
    AttributeNotFilterable {
        /// The public resource type.
        resource_type: String,
        /// The requested public attribute name.
        field: String,
    },
}

impl fmt::Display for FilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { position, message } => {
                write!(formatter, "malformed filter at byte {position}: {message}")
            }
            Self::UnsupportedOperator(operator) => {
                write!(formatter, "filter operator `{operator}` is not supported")
            }
            Self::UnsupportedPath(path) => {
                write!(formatter, "filter path `{path}` is not supported")
            }
            Self::UnknownResourceType(resource_type) => {
                write!(
                    formatter,
                    "resource type `{resource_type}` is not registered"
                )
            }
            Self::UnknownAttribute {
                resource_type,
                field,
            } => write!(
                formatter,
                "attribute `{field}` is not registered on resource `{resource_type}`"
            ),
            Self::AttributeNotFilterable {
                resource_type,
                field,
            } => write!(
                formatter,
                "attribute `{field}` on resource `{resource_type}` is not filterable"
            ),
        }
    }
}

impl std::error::Error for FilterError {}

/// Parses one filter expression and resolves each public attribute mapping.
///
/// Supported forms are `equals(field,'string')`, `equals(field,null)`,
/// `and(expr,expr,...)`, `or(expr,expr,...)`, and `not(expr)`. String literals
/// escape an apostrophe by doubling it.
///
/// # Errors
///
/// Returns an error for malformed syntax, unsupported operators or paths, or
/// fields that are unknown or not explicitly filterable.
pub fn parse_filter(
    registry: &ResourceRegistry,
    resource_type: &str,
    input: &str,
) -> Result<FilterExpression, FilterError> {
    let mut parser = Parser {
        input,
        position: 0,
        registry,
        resource_type,
    };
    let expression = parser.parse_expression()?;
    parser.skip_whitespace();
    if !parser.is_at_end() {
        return Err(parser.malformed("unexpected trailing input"));
    }
    Ok(expression)
}

/// Plans all repeated filter parameters for the same resource scope.
///
/// No parameters produce `None`, one parameter remains unchanged, and two or
/// more parameters are combined using OR. The returned tree contains mapped
/// internal model field names and does not evaluate any filters.
///
/// # Errors
///
/// Returns the first parsing or mapping error encountered.
pub fn plan_filters(
    registry: &ResourceRegistry,
    resource_type: &str,
    filters: &[&str],
) -> Result<Option<FilterExpression>, FilterError> {
    let mut expressions = filters
        .iter()
        .map(|filter| parse_filter(registry, resource_type, filter));
    let Some(first) = expressions.next() else {
        return Ok(None);
    };
    let first = first?;
    let remaining = expressions.collect::<Result<Vec<_>, _>>()?;
    if remaining.is_empty() {
        return Ok(Some(first));
    }
    let mut children = Vec::with_capacity(remaining.len() + 1);
    children.push(first);
    children.extend(remaining);
    Ok(Some(FilterExpression::Or(children)))
}

/// Raw, decoded query parameters for one collection read.
///
/// The HTTP layer is responsible for decoding URL encoding and collecting
/// repeated values. Unknown parameter names are supplied in
/// `unsupported_parameters` so the planner can reject them deterministically.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadQuery {
    /// Repeated filter values at the root resource scope.
    pub filters: Vec<String>,
    /// Comma-separated public sort fields, optionally prefixed with `-`.
    pub sort: Option<String>,
    /// One-based requested page number.
    pub page_number: Option<String>,
    /// Requested positive page size.
    pub page_size: Option<String>,
    /// Sparse fieldsets keyed by public resource type.
    pub fieldsets: BTreeMap<String, String>,
    /// Repeated, comma-separated include paths.
    pub includes: Vec<String>,
    /// Query parameter names that this planner does not support.
    pub unsupported_parameters: Vec<String>,
}

/// Explicit pagination policy for a read planner.
///
/// Defaults and limits are required inputs, not hidden framework defaults.
/// An optional maximum offset lets applications bound expensive deep pages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaginationConfig {
    default_page_number: u64,
    default_page_size: u64,
    maximum_page_size: Option<u64>,
    maximum_offset: Option<u64>,
}

impl PaginationConfig {
    /// Creates pagination policy using application-supplied defaults and limits.
    ///
    /// # Errors
    ///
    /// Returns [`ReadPlanError::InvalidPaginationConfig`] if a required value
    /// is zero, a maximum page size is below the default, the default offset
    /// overflows, or the default offset exceeds its configured maximum.
    pub fn new(
        default_page_number: u64,
        default_page_size: u64,
        maximum_page_size: Option<u64>,
        maximum_offset: Option<u64>,
    ) -> Result<Self, ReadPlanError> {
        if default_page_number == 0 || default_page_size == 0 {
            return Err(ReadPlanError::InvalidPaginationConfig(
                "default page number and size must be positive",
            ));
        }
        if maximum_page_size.is_some_and(|maximum| maximum == 0 || maximum < default_page_size) {
            return Err(ReadPlanError::InvalidPaginationConfig(
                "maximum page size must be positive and at least the default size",
            ));
        }
        let default_offset = (default_page_number - 1)
            .checked_mul(default_page_size)
            .ok_or(ReadPlanError::InvalidPaginationConfig(
                "default page offset overflows",
            ))?;
        if maximum_offset.is_some_and(|maximum| default_offset > maximum) {
            return Err(ReadPlanError::InvalidPaginationConfig(
                "default page offset exceeds the configured maximum",
            ));
        }
        Ok(Self {
            default_page_number,
            default_page_size,
            maximum_page_size,
            maximum_offset,
        })
    }
}

/// The direction of one planned sort term.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    /// Sort values from lowest to highest.
    Ascending,
    /// Sort values from highest to lowest.
    Descending,
}

/// One validated sort term mapped to an internal model field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortField {
    /// The public attribute name supplied by the request.
    pub public_name: String,
    /// The internal model field selected by the registry.
    pub model_field: String,
    /// The requested sort direction.
    pub direction: SortDirection,
}

/// A validated page translated to database offset and limit values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Page {
    /// The one-based page number.
    pub number: u64,
    /// The positive number of resources in this page.
    pub size: u64,
    /// The zero-based offset suitable for database execution.
    pub offset: u64,
    /// The maximum number of resources to fetch.
    pub limit: u64,
}

/// A public resource field resolved to its internal mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedField {
    /// A declared public attribute.
    Attribute {
        /// The public attribute name.
        public_name: String,
        /// The mapped internal model field.
        model_field: String,
    },
    /// A declared public relationship.
    Relationship {
        /// The public relationship name.
        public_name: String,
        /// The mapped internal relation field.
        model_field: String,
        /// The target public resource type.
        target_type: String,
    },
}

/// One relationship edge in a validated include tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncludeNode {
    /// The public relationship name.
    pub public_name: String,
    /// The mapped internal relation field.
    pub model_field: String,
    /// The target public resource type.
    pub target_type: String,
    /// Nested relationships requested from the target resource.
    pub children: Vec<Self>,
}

/// An adapter-independent, validated collection read plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadPlan {
    /// The public root resource type.
    pub resource_type: String,
    /// The parsed root-scope filter, if supplied.
    pub filter: Option<FilterExpression>,
    /// Validated sort terms in request order.
    pub sort: Vec<SortField>,
    /// The configured page translated to offset and limit.
    pub page: Page,
    /// Sparse fieldsets by public resource type.
    ///
    /// A missing entry means the request did not constrain that type's fields.
    pub fieldsets: BTreeMap<String, Vec<PlannedField>>,
    /// Requested relationships as a de-duplicated include tree.
    pub includes: Vec<IncludeNode>,
}

/// An invalid read query or pagination policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadPlanError {
    /// One or more unsupported parameter names, sorted and de-duplicated.
    UnsupportedQueryParameters(Vec<String>),
    /// A resource type is not present in the registry.
    UnknownResourceType(String),
    /// A filter could not be parsed or mapped.
    Filter(FilterError),
    /// A sort expression is empty or malformed.
    InvalidSort(String),
    /// A sort field is repeated.
    DuplicateSortField(String),
    /// A sort field is not a declared public attribute.
    UnknownSortAttribute {
        /// The public resource type.
        resource_type: String,
        /// The unregistered public attribute.
        field: String,
    },
    /// A sort field is not explicitly enabled for sorting.
    AttributeNotSortable {
        /// The public resource type.
        resource_type: String,
        /// The public attribute.
        field: String,
    },
    /// A fieldset names an undeclared attribute or relationship.
    UnknownFieldsetField {
        /// The public resource type.
        resource_type: String,
        /// The undeclared public field.
        field: String,
    },
    /// A fieldset contains an empty or repeated field.
    InvalidFieldset {
        /// The public resource type.
        resource_type: String,
        /// The fieldset value.
        value: String,
    },
    /// An include path has empty or malformed segments.
    InvalidIncludePath(String),
    /// An include path names an undeclared relationship.
    UnknownRelationship {
        /// The resource type at the path segment.
        resource_type: String,
        /// The undeclared public relationship.
        relationship: String,
    },
    /// A pagination value is not a positive unsigned integer.
    InvalidPageParameter {
        /// The parameter name.
        parameter: &'static str,
        /// The supplied value.
        value: String,
    },
    /// An application-supplied pagination policy is invalid.
    InvalidPaginationConfig(&'static str),
    /// The requested page size exceeds the configured maximum.
    PageSizeExceedsMaximum {
        /// The requested page size.
        requested: u64,
        /// The configured maximum.
        maximum: u64,
    },
    /// The requested offset exceeds the configured maximum.
    PageOffsetExceedsMaximum {
        /// The computed offset.
        requested: u64,
        /// The configured maximum.
        maximum: u64,
    },
    /// The requested page offset cannot be represented as `u64`.
    PageOffsetOverflow,
}

impl fmt::Display for ReadPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedQueryParameters(parameters) => write!(
                formatter,
                "unsupported query parameters: {}",
                parameters.join(", ")
            ),
            Self::UnknownResourceType(resource_type) => {
                write!(
                    formatter,
                    "resource type `{resource_type}` is not registered"
                )
            }
            Self::Filter(error) => error.fmt(formatter),
            Self::InvalidSort(value) => write!(formatter, "invalid sort value `{value}`"),
            Self::DuplicateSortField(field) => {
                write!(formatter, "sort field `{field}` is repeated")
            }
            Self::UnknownSortAttribute {
                resource_type,
                field,
            } => write!(
                formatter,
                "sort attribute `{field}` is not registered on resource `{resource_type}`"
            ),
            Self::AttributeNotSortable {
                resource_type,
                field,
            } => write!(
                formatter,
                "attribute `{field}` on resource `{resource_type}` is not sortable"
            ),
            Self::UnknownFieldsetField {
                resource_type,
                field,
            } => write!(
                formatter,
                "fieldset field `{field}` is not registered on resource `{resource_type}`"
            ),
            Self::InvalidFieldset {
                resource_type,
                value,
            } => write!(
                formatter,
                "invalid fieldset `{value}` for resource `{resource_type}`"
            ),
            Self::InvalidIncludePath(path) => write!(formatter, "invalid include path `{path}`"),
            Self::UnknownRelationship {
                resource_type,
                relationship,
            } => write!(
                formatter,
                "relationship `{relationship}` is not registered on resource `{resource_type}`"
            ),
            Self::InvalidPageParameter { parameter, value } => {
                write!(formatter, "invalid `{parameter}` value `{value}`")
            }
            Self::InvalidPaginationConfig(message) => {
                write!(formatter, "invalid pagination configuration: {message}")
            }
            Self::PageSizeExceedsMaximum { requested, maximum } => write!(
                formatter,
                "page size {requested} exceeds configured maximum {maximum}"
            ),
            Self::PageOffsetExceedsMaximum { requested, maximum } => write!(
                formatter,
                "page offset {requested} exceeds configured maximum {maximum}"
            ),
            Self::PageOffsetOverflow => formatter.write_str("page offset overflows"),
        }
    }
}

impl std::error::Error for ReadPlanError {}

impl From<FilterError> for ReadPlanError {
    fn from(error: FilterError) -> Self {
        Self::Filter(error)
    }
}

/// Validates decoded query parameters and builds a persistence-independent read plan.
///
/// The supplied pagination policy is mandatory, so page defaults and limits
/// are explicit application choices. Unknown parameters are rejected before
/// any query component is planned.
///
/// # Errors
///
/// Returns an error for unsupported parameters, invalid filters/sorts/pages,
/// undeclared fieldset or include fields, or a resource type absent from the
/// registry.
pub fn plan_read(
    registry: &ResourceRegistry,
    resource_type: &str,
    query: &ReadQuery,
    pagination: &PaginationConfig,
) -> Result<ReadPlan, ReadPlanError> {
    registry
        .resource(resource_type)
        .map_err(|_| ReadPlanError::UnknownResourceType(resource_type.to_owned()))?;

    let unsupported = query
        .unsupported_parameters
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if !unsupported.is_empty() {
        return Err(ReadPlanError::UnsupportedQueryParameters(
            unsupported.into_iter().collect(),
        ));
    }

    let filter = plan_filters(
        registry,
        resource_type,
        &query.filters.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    let sort = plan_sort(registry, resource_type, query.sort.as_deref())?;
    let page = plan_page(query, pagination)?;
    let fieldsets = plan_fieldsets(registry, &query.fieldsets)?;
    let includes = plan_includes(registry, resource_type, &query.includes)?;

    Ok(ReadPlan {
        resource_type: resource_type.to_owned(),
        filter,
        sort,
        page,
        fieldsets,
        includes,
    })
}

/// Plans a single-resource read, supporting includes and sparse fieldsets.
///
/// Collection filters, sorting, and pagination do not apply to a resource
/// addressed by identifier and are rejected alongside unknown parameters.
/// Fieldsets for any registered resource type are supported so included
/// resources can be projected independently.
///
/// # Errors
///
/// Returns an error for collection-only or unsupported parameters, invalid
/// fieldsets/includes, or a resource type absent from the registry.
pub fn plan_resource_read(
    registry: &ResourceRegistry,
    resource_type: &str,
    query: &ReadQuery,
    pagination: &PaginationConfig,
) -> Result<ReadPlan, ReadPlanError> {
    registry
        .resource(resource_type)
        .map_err(|_| ReadPlanError::UnknownResourceType(resource_type.to_owned()))?;

    let mut unsupported = query
        .unsupported_parameters
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if !query.filters.is_empty() {
        unsupported.insert("filter".to_owned());
    }
    if query.sort.is_some() {
        unsupported.insert("sort".to_owned());
    }
    if query.page_number.is_some() {
        unsupported.insert("page[number]".to_owned());
    }
    if query.page_size.is_some() {
        unsupported.insert("page[size]".to_owned());
    }
    if !unsupported.is_empty() {
        return Err(ReadPlanError::UnsupportedQueryParameters(
            unsupported.into_iter().collect(),
        ));
    }

    let mut plan = plan_read(registry, resource_type, query, pagination)?;
    plan.page = Page {
        number: 1,
        size: 1,
        offset: 0,
        limit: 1,
    };
    Ok(plan)
}

fn plan_sort(
    registry: &ResourceRegistry,
    resource_type: &str,
    input: Option<&str>,
) -> Result<Vec<SortField>, ReadPlanError> {
    let Some(input) = input else {
        return Ok(Vec::new());
    };
    if input.is_empty() {
        return Err(ReadPlanError::InvalidSort(input.to_owned()));
    }
    let definition = registry
        .resource(resource_type)
        .map_err(|_| ReadPlanError::UnknownResourceType(resource_type.to_owned()))?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for term in input.split(',') {
        let (direction, field) = match term.strip_prefix('-') {
            Some(field) => (SortDirection::Descending, field),
            None => (SortDirection::Ascending, term),
        };
        if field.is_empty() || field.trim() != field || field.starts_with('+') {
            return Err(ReadPlanError::InvalidSort(input.to_owned()));
        }
        if !seen.insert(field.to_owned()) {
            return Err(ReadPlanError::DuplicateSortField(field.to_owned()));
        }
        let attribute = definition.attribute_by_name(field).ok_or_else(|| {
            ReadPlanError::UnknownSortAttribute {
                resource_type: resource_type.to_owned(),
                field: field.to_owned(),
            }
        })?;
        if !attribute.is_sortable() {
            return Err(ReadPlanError::AttributeNotSortable {
                resource_type: resource_type.to_owned(),
                field: field.to_owned(),
            });
        }
        result.push(SortField {
            public_name: field.to_owned(),
            model_field: attribute.model_field().to_owned(),
            direction,
        });
    }
    Ok(result)
}

fn plan_page(query: &ReadQuery, config: &PaginationConfig) -> Result<Page, ReadPlanError> {
    let number = parse_page_value(query.page_number.as_deref(), "page[number]")?
        .unwrap_or(config.default_page_number);
    let size = parse_page_value(query.page_size.as_deref(), "page[size]")?
        .unwrap_or(config.default_page_size);
    if let Some(maximum) = config.maximum_page_size
        && size > maximum
    {
        return Err(ReadPlanError::PageSizeExceedsMaximum {
            requested: size,
            maximum,
        });
    }
    let offset = (number - 1)
        .checked_mul(size)
        .ok_or(ReadPlanError::PageOffsetOverflow)?;
    if let Some(maximum) = config.maximum_offset
        && offset > maximum
    {
        return Err(ReadPlanError::PageOffsetExceedsMaximum {
            requested: offset,
            maximum,
        });
    }
    Ok(Page {
        number,
        size,
        offset,
        limit: size,
    })
}

fn parse_page_value(
    input: Option<&str>,
    parameter: &'static str,
) -> Result<Option<u64>, ReadPlanError> {
    let Some(input) = input else {
        return Ok(None);
    };
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ReadPlanError::InvalidPageParameter {
            parameter,
            value: input.to_owned(),
        });
    }
    let value = input
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ReadPlanError::InvalidPageParameter {
            parameter,
            value: input.to_owned(),
        })?;
    Ok(Some(value))
}

fn plan_fieldsets(
    registry: &ResourceRegistry,
    fieldsets: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<PlannedField>>, ReadPlanError> {
    let mut planned = BTreeMap::new();
    for (resource_type, value) in fieldsets {
        let definition = registry
            .resource(resource_type)
            .map_err(|_| ReadPlanError::UnknownResourceType(resource_type.clone()))?;
        let mut seen = BTreeSet::new();
        let mut fields = Vec::new();
        if value.is_empty() {
            planned.insert(resource_type.clone(), fields);
            continue;
        }
        for field in value.split(',') {
            if field.is_empty() || field.trim() != field || !seen.insert(field) {
                return Err(ReadPlanError::InvalidFieldset {
                    resource_type: resource_type.clone(),
                    value: value.clone(),
                });
            }
            if let Some(attribute) = definition.attribute_by_name(field) {
                fields.push(PlannedField::Attribute {
                    public_name: field.to_owned(),
                    model_field: attribute.model_field().to_owned(),
                });
            } else if let Some(relationship) = definition.relationship_by_name(field) {
                fields.push(PlannedField::Relationship {
                    public_name: field.to_owned(),
                    model_field: relationship.model_field().to_owned(),
                    target_type: relationship.target_type().to_owned(),
                });
            } else {
                return Err(ReadPlanError::UnknownFieldsetField {
                    resource_type: resource_type.clone(),
                    field: field.to_owned(),
                });
            }
        }
        planned.insert(resource_type.clone(), fields);
    }
    Ok(planned)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MutableIncludeNode {
    model_field: String,
    target_type: String,
    children: BTreeMap<String, Self>,
}

fn plan_includes(
    registry: &ResourceRegistry,
    root_type: &str,
    include_values: &[String],
) -> Result<Vec<IncludeNode>, ReadPlanError> {
    let mut tree = BTreeMap::<String, MutableIncludeNode>::new();
    for value in include_values {
        if value.is_empty() {
            continue;
        }
        for path in value.split(',') {
            let segments = path.split('.').collect::<Vec<_>>();
            if segments
                .iter()
                .any(|segment| segment.is_empty() || segment.trim() != *segment)
            {
                return Err(ReadPlanError::InvalidIncludePath(path.to_owned()));
            }
            let mut definition = registry
                .resource(root_type)
                .map_err(|_| ReadPlanError::UnknownResourceType(root_type.to_owned()))?;
            let mut children = &mut tree;
            for segment in segments {
                let relationship = definition.relationship_by_name(segment).ok_or_else(|| {
                    ReadPlanError::UnknownRelationship {
                        resource_type: definition.type_name().to_owned(),
                        relationship: segment.to_owned(),
                    }
                })?;
                let node =
                    children
                        .entry(segment.to_owned())
                        .or_insert_with(|| MutableIncludeNode {
                            model_field: relationship.model_field().to_owned(),
                            target_type: relationship.target_type().to_owned(),
                            children: BTreeMap::new(),
                        });
                definition = registry.resource(relationship.target_type()).map_err(|_| {
                    ReadPlanError::UnknownResourceType(relationship.target_type().to_owned())
                })?;
                children = &mut node.children;
            }
        }
    }
    Ok(tree
        .into_iter()
        .map(|(public_name, node)| freeze_include(public_name, node))
        .collect())
}

fn freeze_include(public_name: String, node: MutableIncludeNode) -> IncludeNode {
    IncludeNode {
        public_name,
        model_field: node.model_field,
        target_type: node.target_type,
        children: node
            .children
            .into_iter()
            .map(|(name, child)| freeze_include(name, child))
            .collect(),
    }
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
    registry: &'a ResourceRegistry,
    resource_type: &'a str,
}

impl Parser<'_> {
    fn parse_expression(&mut self) -> Result<FilterExpression, FilterError> {
        self.skip_whitespace();
        let operator = self.parse_identifier()?;
        self.skip_whitespace();
        self.expect_byte(b'(')?;

        match operator.as_str() {
            "equals" => self.parse_equals(),
            "and" => self.parse_group(true),
            "or" => self.parse_group(false),
            "not" => self.parse_not(),
            _ => Err(FilterError::UnsupportedOperator(operator)),
        }
    }

    fn parse_equals(&mut self) -> Result<FilterExpression, FilterError> {
        self.skip_whitespace();
        let field = self.parse_field()?;
        self.skip_whitespace();
        self.expect_byte(b',')?;
        self.skip_whitespace();
        let value = if self.peek_byte() == Some(b'\'') {
            FilterValue::String(self.parse_string()?)
        } else {
            let value = self.parse_identifier()?;
            if value != "null" {
                return Err(self.malformed("expected a quoted string or null"));
            }
            FilterValue::Null
        };
        self.skip_whitespace();
        self.expect_byte(b')')?;
        let model_field = self.resolve_field(&field)?;
        Ok(FilterExpression::Equals { model_field, value })
    }

    fn parse_group(&mut self, is_and: bool) -> Result<FilterExpression, FilterError> {
        let mut children = Vec::new();
        self.skip_whitespace();
        if self.peek_byte() != Some(b')') {
            loop {
                children.push(self.parse_expression()?);
                self.skip_whitespace();
                match self.peek_byte() {
                    Some(b',') => {
                        self.position += 1;
                        self.skip_whitespace();
                    }
                    Some(b')') => break,
                    _ => return Err(self.malformed("expected `,` or `)`")),
                }
            }
        }
        self.expect_byte(b')')?;
        if children.len() < 2 {
            return Err(self.malformed("and/or requires at least two expressions"));
        }
        Ok(if is_and {
            FilterExpression::And(children)
        } else {
            FilterExpression::Or(children)
        })
    }

    fn parse_not(&mut self) -> Result<FilterExpression, FilterError> {
        self.skip_whitespace();
        if self.peek_byte() == Some(b')') {
            return Err(self.malformed("not requires exactly one expression"));
        }
        let child = self.parse_expression()?;
        self.skip_whitespace();
        if self.peek_byte() == Some(b',') {
            return Err(self.malformed("not requires exactly one expression"));
        }
        self.expect_byte(b')')?;
        Ok(FilterExpression::Not(Box::new(child)))
    }

    fn parse_field(&mut self) -> Result<String, FilterError> {
        let start = self.position;
        while let Some(byte) = self.peek_byte() {
            if byte == b',' || byte == b')' {
                break;
            }
            self.position += 1;
        }
        let field = self.input[start..self.position].trim();
        if field.is_empty()
            || field
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'(' | b'\'' | b'"'))
        {
            return Err(self.malformed("expected a public attribute name"));
        }
        if field.contains('.') {
            return Err(FilterError::UnsupportedPath(field.to_owned()));
        }
        Ok(field.to_owned())
    }

    fn parse_string(&mut self) -> Result<String, FilterError> {
        self.expect_byte(b'\'')?;
        let mut value = String::new();
        loop {
            let Some(byte) = self.peek_byte() else {
                return Err(self.malformed("unterminated string literal"));
            };
            if byte == b'\'' {
                self.position += 1;
                if self.peek_byte() == Some(b'\'') {
                    self.position += 1;
                    value.push('\'');
                } else {
                    return Ok(value);
                }
            } else {
                let character = self.input[self.position..]
                    .chars()
                    .next()
                    .expect("position is within input");
                value.push(character);
                self.position += character.len_utf8();
            }
        }
    }

    fn parse_identifier(&mut self) -> Result<String, FilterError> {
        let start = self.position;
        while let Some(byte) = self.peek_byte() {
            if !byte.is_ascii_alphanumeric() && byte != b'_' && byte != b'-' {
                break;
            }
            self.position += 1;
        }
        if self.position == start {
            return Err(self.malformed("expected an operator or literal"));
        }
        Ok(self.input[start..self.position].to_owned())
    }

    fn resolve_field(&self, field: &str) -> Result<String, FilterError> {
        self.registry
            .resource(self.resource_type)
            .map_err(|error| match error {
                RegistryError::UnknownResourceType(name) => FilterError::UnknownResourceType(name),
                _ => unreachable!("resource lookup only returns unknown-resource errors"),
            })?;
        match self
            .registry
            .filterable_attribute(self.resource_type, field)
        {
            Ok(attribute) => Ok(attribute.model_field().to_owned()),
            Err(RegistryError::AttributeNotFilterable { .. }) => {
                Err(FilterError::AttributeNotFilterable {
                    resource_type: self.resource_type.to_owned(),
                    field: field.to_owned(),
                })
            }
            Err(RegistryError::UnknownField { .. }) => {
                let resource = self
                    .registry
                    .resource(self.resource_type)
                    .expect("resource type was validated above");
                if resource.relationship_by_name(field).is_some() {
                    Err(FilterError::UnsupportedPath(field.to_owned()))
                } else {
                    Err(FilterError::UnknownAttribute {
                        resource_type: self.resource_type.to_owned(),
                        field: field.to_owned(),
                    })
                }
            }
            Err(RegistryError::UnknownResourceType(_)) => {
                unreachable!("resource type was validated above")
            }
            Err(_) => unreachable!("filterable attribute lookup returned an unrelated error"),
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek_byte()
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.position += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), FilterError> {
        if self.peek_byte() == Some(expected) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.malformed(match expected {
                b'(' => "expected `(`",
                b')' => "expected `)`",
                b',' => "expected `,`",
                _ => "unexpected character",
            }))
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn is_at_end(&self) -> bool {
        self.position == self.input.len()
    }

    fn malformed(&self, message: &'static str) -> FilterError {
        FilterError::Malformed {
            position: self.position,
            message,
        }
    }
}
