//! Explicit public resource metadata, independent of persistence entities.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

use crate::document::is_valid_member_name;

/// An explicitly declared public JSON:API resource type.
///
/// Public fields map explicitly to internal field names. SeaORM helpers can
/// bind these names to typed columns, but the registry remains independent of
/// the persistence entity. Public fields cannot alias each other or the
/// identifier's internal field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceDefinition {
    type_name: String,
    identifier_field: String,
    attributes: Vec<AttributeMapping>,
    relationships: Vec<RelationshipMapping>,
    permissions: BTreeSet<ResourcePermission>,
}

impl ResourceDefinition {
    /// Creates a resource definition with its public type and internal
    /// identifier field.
    #[must_use]
    pub fn new(type_name: impl Into<String>, identifier_field: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
            identifier_field: identifier_field.into(),
            attributes: Vec::new(),
            relationships: Vec::new(),
            permissions: BTreeSet::new(),
        }
    }

    /// Enables one resource-level operation. Registered resources are
    /// exposed for reads; mutations, including Atomic Operations, are denied
    /// until individually enabled here.
    #[must_use]
    pub fn allow(mut self, permission: ResourcePermission) -> Self {
        self.permissions.insert(permission);
        self
    }

    /// Returns whether the resource-level operation was explicitly enabled.
    #[must_use]
    pub fn allows(&self, permission: ResourcePermission) -> bool {
        self.permissions.contains(&permission)
    }

    /// Adds an explicitly configured typed field mapping.
    #[must_use]
    pub fn mapped_attribute(mut self, mapping: AttributeMapping) -> Self {
        self.attributes.push(mapping);
        self
    }

    /// Adds an explicitly configured relationship mapping.
    #[must_use]
    pub fn mapped_relationship(mut self, mapping: RelationshipMapping) -> Self {
        self.relationships.push(mapping);
        self
    }

    /// Declares a public attribute and its internal field mapping.
    ///
    /// Filtering and sorting are disabled unless explicitly enabled here.
    #[must_use]
    pub fn attribute(
        mut self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
    ) -> Self {
        self.attributes
            .push(AttributeMapping::new(public_name, model_field));
        self
    }

    /// Declares a public attribute that can be filtered but not sorted.
    #[must_use]
    pub fn filterable_attribute(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
    ) -> Self {
        self.mapped_attribute(
            AttributeMapping::new(public_name, model_field).allow(AttributePermission::Filter),
        )
    }

    /// Declares a public attribute that can be sorted but not filtered.
    #[must_use]
    pub fn sortable_attribute(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
    ) -> Self {
        self.mapped_attribute(
            AttributeMapping::new(public_name, model_field).allow(AttributePermission::Sort),
        )
    }

    /// Declares a public attribute that can be filtered and sorted.
    #[must_use]
    pub fn filterable_and_sortable_attribute(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
    ) -> Self {
        self.mapped_attribute(
            AttributeMapping::new(public_name, model_field)
                .allow(AttributePermission::Filter)
                .allow(AttributePermission::Sort),
        )
    }

    /// Declares a public relationship and its internal field and target type.
    #[must_use]
    pub fn relationship(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
        target_type: impl Into<String>,
    ) -> Self {
        self.relationship_with_cardinality(
            public_name.into(),
            model_field.into(),
            target_type.into(),
            None,
        )
    }

    /// Declares a to-one relationship and its internal field and target type.
    #[must_use]
    pub fn to_one_relationship(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
        target_type: impl Into<String>,
    ) -> Self {
        self.relationship_with_cardinality(
            public_name.into(),
            model_field.into(),
            target_type.into(),
            Some(RelationshipCardinality::ToOne),
        )
    }

    /// Declares a to-many relationship and its internal field and target type.
    #[must_use]
    pub fn to_many_relationship(
        self,
        public_name: impl Into<String>,
        model_field: impl Into<String>,
        target_type: impl Into<String>,
    ) -> Self {
        self.relationship_with_cardinality(
            public_name.into(),
            model_field.into(),
            target_type.into(),
            Some(RelationshipCardinality::ToMany),
        )
    }

    fn relationship_with_cardinality(
        mut self,
        public_name: String,
        model_field: String,
        target_type: String,
        cardinality: Option<RelationshipCardinality>,
    ) -> Self {
        self.relationships.push(RelationshipMapping {
            public_name,
            model_field,
            target_type,
            cardinality,
            storage: RelationshipStorage::Custom,
            permissions: BTreeSet::from([RelationshipPermission::Read]),
        });
        self
    }

    /// Returns the public resource type name.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Returns the mapped internal identifier field.
    #[must_use]
    pub fn identifier_field(&self) -> &str {
        &self.identifier_field
    }

    /// Returns the declared public attributes.
    #[must_use]
    pub fn attributes(&self) -> &[AttributeMapping] {
        &self.attributes
    }

    /// Returns the declared public relationships.
    #[must_use]
    pub fn relationships(&self) -> &[RelationshipMapping] {
        &self.relationships
    }

    /// Looks up a declared public attribute by name.
    #[must_use]
    pub fn attribute_by_name(&self, public_name: &str) -> Option<&AttributeMapping> {
        self.attributes
            .iter()
            .find(|attribute| attribute.public_name == public_name)
    }

    /// Looks up a declared public relationship by name.
    #[must_use]
    pub fn relationship_by_name(&self, public_name: &str) -> Option<&RelationshipMapping> {
        self.relationships
            .iter()
            .find(|relationship| relationship.public_name == public_name)
    }

    fn validate(&self) -> Result<(), RegistryError> {
        if self.type_name.is_empty() {
            return Err(RegistryError::EmptyResourceType);
        }
        if !is_valid_member_name(&self.type_name) {
            return Err(RegistryError::InvalidResourceTypeName(
                self.type_name.clone(),
            ));
        }
        if self.identifier_field.is_empty() {
            return Err(RegistryError::EmptyIdentifierField {
                resource_type: self.type_name.clone(),
            });
        }

        let mut public_names = HashSet::new();
        let mut model_fields = HashSet::from([self.identifier_field.clone()]);
        for attribute in &self.attributes {
            validate_field_name(
                &self.type_name,
                &attribute.public_name,
                &attribute.model_field,
                &mut public_names,
                &mut model_fields,
            )?;
        }
        for relationship in &self.relationships {
            validate_field_name(
                &self.type_name,
                &relationship.public_name,
                &relationship.model_field,
                &mut public_names,
                &mut model_fields,
            )?;
            if relationship.target_type.is_empty() {
                return Err(RegistryError::EmptyRelationshipTarget {
                    resource_type: self.type_name.clone(),
                    relationship: relationship.public_name.clone(),
                });
            }
            let storage_error = match &relationship.storage {
                RelationshipStorage::Custom => None,
                RelationshipStorage::ToOneForeignKey { .. }
                    if relationship.cardinality != Some(RelationshipCardinality::ToOne) =>
                {
                    Some("source-side foreign keys require to-one cardinality".to_owned())
                }
                RelationshipStorage::ToManyForeignKey { .. }
                    if relationship.cardinality != Some(RelationshipCardinality::ToMany) =>
                {
                    Some("target-side foreign keys require to-many cardinality".to_owned())
                }
                RelationshipStorage::JoinTable { .. }
                    if relationship.cardinality != Some(RelationshipCardinality::ToMany) =>
                {
                    Some("join tables require to-many cardinality".to_owned())
                }
                RelationshipStorage::ToManyForeignKey {
                    foreign_key_field, ..
                } if foreign_key_field.is_empty() => {
                    Some("target foreign-key field must not be empty".to_owned())
                }
                RelationshipStorage::JoinTable {
                    source_column,
                    target_column,
                } if source_column.is_empty()
                    || target_column.is_empty()
                    || source_column == target_column =>
                {
                    Some("join-table columns must be non-empty and distinct".to_owned())
                }
                _ => None,
            };
            if let Some(reason) = storage_error {
                return Err(RegistryError::InvalidRelationshipStorage {
                    resource_type: self.type_name.clone(),
                    relationship: relationship.public_name.clone(),
                    reason,
                });
            }
        }
        Ok(())
    }
}

/// A resource-level operation permission.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResourcePermission {
    /// Create through ordinary JSON:API HTTP mutations.
    Create,
    /// Update through ordinary JSON:API HTTP mutations.
    Update,
    /// Delete through ordinary JSON:API HTTP mutations.
    Delete,
    /// Add a resource through Atomic Operations.
    AtomicCreate,
    /// Update a resource through Atomic Operations.
    AtomicUpdate,
    /// Remove a resource through Atomic Operations.
    AtomicDelete,
}

/// A field-level permission for a public attribute.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AttributePermission {
    /// Return the attribute in a resource representation.
    Read,
    /// Allow this attribute in collection filters.
    Filter,
    /// Allow this attribute in collection sort expressions.
    Sort,
    /// Set this attribute through ordinary resource creation.
    Create,
    /// Change this attribute through ordinary resource updates.
    Update,
    /// Set this attribute through Atomic resource adds.
    AtomicCreate,
    /// Change this attribute through Atomic resource updates.
    AtomicUpdate,
}

impl AttributeMapping {
    /// Creates a read-only public attribute mapping.
    #[must_use]
    pub fn new(public_name: impl Into<String>, model_field: impl Into<String>) -> Self {
        Self {
            public_name: public_name.into(),
            model_field: model_field.into(),
            permissions: BTreeSet::from([AttributePermission::Read]),
        }
    }

    /// Enables a named field capability.
    #[must_use]
    pub fn allow(mut self, permission: AttributePermission) -> Self {
        self.permissions.insert(permission);
        self
    }

    /// Returns whether a named field capability was explicitly enabled.
    #[must_use]
    pub fn allows(&self, permission: AttributePermission) -> bool {
        self.permissions.contains(&permission)
    }
}

/// A public attribute mapped to an internal field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributeMapping {
    public_name: String,
    model_field: String,
    permissions: BTreeSet<AttributePermission>,
}

impl AttributeMapping {
    /// Returns the public JSON:API attribute name.
    #[must_use]
    pub fn public_name(&self) -> &str {
        &self.public_name
    }

    /// Returns the internal field name.
    #[must_use]
    pub fn model_field(&self) -> &str {
        &self.model_field
    }

    /// Returns whether filtering is explicitly enabled for this attribute.
    #[must_use]
    pub fn is_filterable(&self) -> bool {
        self.permissions.contains(&AttributePermission::Filter)
    }

    /// Returns whether sorting is explicitly enabled for this attribute.
    #[must_use]
    pub fn is_sortable(&self) -> bool {
        self.permissions.contains(&AttributePermission::Sort)
    }
}

/// A public relationship mapped to an internal field and target resource type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationshipMapping {
    public_name: String,
    model_field: String,
    target_type: String,
    cardinality: Option<RelationshipCardinality>,
    storage: RelationshipStorage,
    permissions: BTreeSet<RelationshipPermission>,
}

/// Persistence shape declared for a relationship.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelationshipStorage {
    /// A foreign key on this resource points to the related resource.
    ToOneForeignKey {
        /// Whether the foreign key may contain SQL `NULL`.
        nullable: bool,
    },
    /// A foreign key on each related resource points back to this resource.
    ToManyForeignKey {
        /// The foreign-key column on the related resource.
        foreign_key_field: String,
        /// Whether the related foreign key may contain SQL `NULL`.
        nullable: bool,
        /// How adding a member already owned by another resource is handled.
        reassignment: RelationshipReassignment,
    },
    /// A two-column join table connects this resource and the related resource.
    JoinTable {
        /// The join-table column containing this resource's identifier.
        source_column: String,
        /// The join-table column containing the related resource's identifier.
        target_column: String,
    },
    /// Persistence is application-defined and requires a custom executor.
    Custom,
}

/// Reassignment policy for a to-many foreign-key relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationshipReassignment {
    /// Reject adding a resource currently owned by a different source.
    Deny,
    /// Move a resource from its current source to the requested source.
    Transfer,
}

/// A relationship-level permission. Linkage reads and include traversal are
/// independent, as are ordinary and Atomic relationship writes.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RelationshipPermission {
    /// Return this relationship's linkage in resource representations.
    Read,
    /// Load this relationship through an `include` query.
    Include,
    /// Fetch linkage from the relationship endpoint.
    LinkageRead,
    /// Fetch related resources from the related-resource endpoint.
    RelatedRead,
    /// Replace linkage through a base HTTP PATCH.
    BaseReplace,
    /// Add members through a base HTTP POST.
    BaseAdd,
    /// Remove members through a base HTTP DELETE.
    BaseRemove,
    /// Replace linkage through an Atomic update.
    AtomicReplace,
    /// Add members through an Atomic add.
    AtomicAdd,
    /// Remove members through an Atomic remove.
    AtomicRemove,
    /// Change linkage as part of a base resource POST.
    ResourceCreate,
    /// Change linkage as part of a base resource PATCH.
    ResourceUpdate,
    /// Change linkage as part of an Atomic resource add.
    AtomicResourceCreate,
    /// Change linkage as part of an Atomic resource update.
    AtomicResourceUpdate,
}

impl RelationshipMapping {
    /// Creates a relationship mapping with no include or write access.
    #[must_use]
    pub fn new(
        public_name: impl Into<String>,
        model_field: impl Into<String>,
        target_type: impl Into<String>,
    ) -> Self {
        Self {
            public_name: public_name.into(),
            model_field: model_field.into(),
            target_type: target_type.into(),
            cardinality: None,
            storage: RelationshipStorage::Custom,
            permissions: BTreeSet::from([RelationshipPermission::Read]),
        }
    }

    /// Declares to-one linkage cardinality.
    #[must_use]
    pub fn to_one(mut self) -> Self {
        self.cardinality = Some(RelationshipCardinality::ToOne);
        self
    }

    /// Declares a source-side foreign key and its SQL nullability.
    #[must_use]
    pub fn to_one_foreign_key(mut self, nullable: bool) -> Self {
        self.cardinality = Some(RelationshipCardinality::ToOne);
        self.storage = RelationshipStorage::ToOneForeignKey { nullable };
        self
    }

    /// Declares to-many linkage cardinality.
    #[must_use]
    pub fn to_many(mut self) -> Self {
        self.cardinality = Some(RelationshipCardinality::ToMany);
        self
    }

    /// Declares a target-side foreign key for a to-many relationship.
    #[must_use]
    pub fn to_many_foreign_key(
        mut self,
        foreign_key_field: impl Into<String>,
        nullable: bool,
        reassignment: RelationshipReassignment,
    ) -> Self {
        self.cardinality = Some(RelationshipCardinality::ToMany);
        self.storage = RelationshipStorage::ToManyForeignKey {
            foreign_key_field: foreign_key_field.into(),
            nullable,
            reassignment,
        };
        self
    }

    /// Declares a two-column join-table mapping for a to-many relationship.
    #[must_use]
    pub fn to_many_join_table(
        mut self,
        source_column: impl Into<String>,
        target_column: impl Into<String>,
    ) -> Self {
        self.cardinality = Some(RelationshipCardinality::ToMany);
        self.storage = RelationshipStorage::JoinTable {
            source_column: source_column.into(),
            target_column: target_column.into(),
        };
        self
    }

    /// Enables a named relationship capability.
    #[must_use]
    pub fn allow(mut self, permission: RelationshipPermission) -> Self {
        self.permissions.insert(permission);
        self
    }

    /// Removes a named relationship capability.
    #[must_use]
    pub fn disallow(mut self, permission: RelationshipPermission) -> Self {
        self.permissions.remove(&permission);
        self
    }

    /// Returns whether a named relationship capability was explicitly enabled.
    #[must_use]
    pub fn allows(&self, permission: RelationshipPermission) -> bool {
        self.permissions.contains(&permission)
    }
}

impl RelationshipMapping {
    /// Returns the public JSON:API relationship name.
    #[must_use]
    pub fn public_name(&self) -> &str {
        &self.public_name
    }

    /// Returns the internal field name.
    #[must_use]
    pub fn model_field(&self) -> &str {
        &self.model_field
    }

    /// Returns the target public resource type.
    #[must_use]
    pub fn target_type(&self) -> &str {
        &self.target_type
    }

    /// Returns the declared relationship cardinality, if provided.
    #[must_use]
    pub const fn cardinality(&self) -> Option<RelationshipCardinality> {
        self.cardinality
    }

    /// Returns the declared persistence shape.
    #[must_use]
    pub fn storage(&self) -> &RelationshipStorage {
        &self.storage
    }
}

/// The declared resource-linkage cardinality for a relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationshipCardinality {
    /// The relationship links to at most one resource identifier.
    ToOne,
    /// The relationship links to zero or more resource identifiers.
    ToMany,
}

/// A registry of explicitly declared public resource types.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourceRegistry {
    resources: BTreeMap<String, ResourceDefinition>,
}

impl ResourceRegistry {
    /// Validates and registers a collection of resource definitions.
    ///
    /// Every relationship target must name a resource in the same registry.
    /// Registration is all-or-nothing: an invalid batch returns no registry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate resource/field declarations
    /// and for relationships whose target resource has not been registered.
    pub fn new(
        definitions: impl IntoIterator<Item = ResourceDefinition>,
    ) -> Result<Self, RegistryError> {
        let mut resources = BTreeMap::new();
        for definition in definitions {
            definition.validate()?;
            let type_name = definition.type_name.clone();
            if resources.contains_key(&type_name) {
                return Err(RegistryError::DuplicateResourceType(type_name));
            }
            resources.insert(type_name, definition);
        }

        for definition in resources.values() {
            for relationship in &definition.relationships {
                if !resources.contains_key(&relationship.target_type) {
                    return Err(RegistryError::UnknownRelationshipTarget {
                        resource_type: definition.type_name.clone(),
                        relationship: relationship.public_name.clone(),
                        target_type: relationship.target_type.clone(),
                    });
                }
            }
        }

        Ok(Self { resources })
    }

    /// Looks up a registered resource type.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::UnknownResourceType`] when the type is not
    /// registered.
    pub fn resource(&self, type_name: &str) -> Result<&ResourceDefinition, RegistryError> {
        self.resources
            .get(type_name)
            .ok_or_else(|| RegistryError::UnknownResourceType(type_name.to_owned()))
    }

    /// Iterates over explicitly registered resource definitions.
    pub fn resources(&self) -> impl Iterator<Item = &ResourceDefinition> {
        self.resources.values()
    }

    /// Looks up an explicitly registered public attribute.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource or attribute is not registered.
    pub fn attribute(
        &self,
        type_name: &str,
        public_name: &str,
    ) -> Result<&AttributeMapping, RegistryError> {
        self.resource(type_name)?
            .attribute_by_name(public_name)
            .ok_or_else(|| RegistryError::UnknownField {
                resource_type: type_name.to_owned(),
                field_name: public_name.to_owned(),
            })
    }

    /// Looks up an attribute only when filtering was explicitly enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource/attribute is unknown or filtering is
    /// disabled for the attribute.
    pub fn filterable_attribute(
        &self,
        type_name: &str,
        public_name: &str,
    ) -> Result<&AttributeMapping, RegistryError> {
        let attribute = self.attribute(type_name, public_name)?;
        if !attribute.allows(AttributePermission::Filter) {
            return Err(RegistryError::AttributeNotFilterable {
                resource_type: type_name.to_owned(),
                field_name: public_name.to_owned(),
            });
        }
        Ok(attribute)
    }

    /// Looks up an attribute only when sorting was explicitly enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource/attribute is unknown or sorting is
    /// disabled for the attribute.
    pub fn sortable_attribute(
        &self,
        type_name: &str,
        public_name: &str,
    ) -> Result<&AttributeMapping, RegistryError> {
        let attribute = self.attribute(type_name, public_name)?;
        if !attribute.allows(AttributePermission::Sort) {
            return Err(RegistryError::AttributeNotSortable {
                resource_type: type_name.to_owned(),
                field_name: public_name.to_owned(),
            });
        }
        Ok(attribute)
    }

    /// Looks up an explicitly registered public relationship.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource or relationship is not registered.
    pub fn relationship(
        &self,
        type_name: &str,
        public_name: &str,
    ) -> Result<&RelationshipMapping, RegistryError> {
        self.resource(type_name)?
            .relationship_by_name(public_name)
            .ok_or_else(|| RegistryError::UnknownField {
                resource_type: type_name.to_owned(),
                field_name: public_name.to_owned(),
            })
    }
}

/// An invalid resource registry declaration or lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// A resource type name is empty.
    EmptyResourceType,
    /// The mapped identifier field is empty.
    EmptyIdentifierField {
        /// The public resource type.
        resource_type: String,
    },
    /// A public field name is empty.
    EmptyFieldName {
        /// The public resource type.
        resource_type: String,
    },
    /// A public field name violates JSON:API member-name rules.
    InvalidFieldName {
        /// The public resource type.
        resource_type: String,
        /// The invalid public field name.
        field_name: String,
    },
    /// A public resource type violates JSON:API member-name rules.
    InvalidResourceTypeName(String),
    /// An internal field mapping is empty.
    EmptyModelField {
        /// The public resource type.
        resource_type: String,
        /// The public field name.
        field_name: String,
    },
    /// A field uses a reserved JSON:API name.
    ReservedFieldName {
        /// The public resource type.
        resource_type: String,
        /// The reserved field name.
        field_name: String,
    },
    /// A public field name is declared more than once.
    DuplicateFieldName {
        /// The public resource type.
        resource_type: String,
        /// The duplicate field name.
        field_name: String,
    },
    /// Multiple public fields map to the same internal field.
    DuplicateModelField {
        /// The public resource type.
        resource_type: String,
        /// The internal field with conflicting mappings.
        model_field: String,
    },
    /// A relationship has an empty target type.
    EmptyRelationshipTarget {
        /// The public resource type.
        resource_type: String,
        /// The relationship name.
        relationship: String,
    },
    /// A relationship storage declaration conflicts with its cardinality or fields.
    InvalidRelationshipStorage {
        /// The public resource type.
        resource_type: String,
        /// The relationship name.
        relationship: String,
        /// The invalid storage detail.
        reason: String,
    },
    /// A public resource type is declared more than once.
    DuplicateResourceType(String),
    /// A relationship references an unregistered target type.
    UnknownRelationshipTarget {
        /// The public resource type declaring the relationship.
        resource_type: String,
        /// The relationship name.
        relationship: String,
        /// The unregistered target type.
        target_type: String,
    },
    /// A lookup references an unregistered resource type.
    UnknownResourceType(String),
    /// A lookup references an unregistered public field.
    UnknownField {
        /// The public resource type.
        resource_type: String,
        /// The unregistered public field name.
        field_name: String,
    },
    /// Filtering was requested for an attribute that is not filterable.
    AttributeNotFilterable {
        /// The public resource type.
        resource_type: String,
        /// The public attribute name.
        field_name: String,
    },
    /// Sorting was requested for an attribute that is not sortable.
    AttributeNotSortable {
        /// The public resource type.
        resource_type: String,
        /// The public attribute name.
        field_name: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyResourceType => formatter.write_str("resource type must not be empty"),
            Self::EmptyIdentifierField { resource_type } => {
                write!(
                    formatter,
                    "resource `{resource_type}` has no identifier field mapping"
                )
            }
            Self::EmptyFieldName { resource_type } => {
                write!(
                    formatter,
                    "resource `{resource_type}` has an empty public field name"
                )
            }
            Self::InvalidFieldName {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "field `{field_name}` on resource `{resource_type}` is not a valid JSON:API member name"
            ),
            Self::InvalidResourceTypeName(resource_type) => write!(
                formatter,
                "resource type `{resource_type}` is not a valid JSON:API member name"
            ),
            Self::EmptyModelField {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "field `{field_name}` on resource `{resource_type}` has no internal mapping"
            ),
            Self::ReservedFieldName {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "field `{field_name}` on resource `{resource_type}` uses a reserved name"
            ),
            Self::DuplicateFieldName {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "field `{field_name}` is duplicated on resource `{resource_type}`"
            ),
            Self::DuplicateModelField {
                resource_type,
                model_field,
            } => write!(
                formatter,
                "internal field `{model_field}` has multiple mappings on resource `{resource_type}`"
            ),
            Self::EmptyRelationshipTarget {
                resource_type,
                relationship,
            } => write!(
                formatter,
                "relationship `{relationship}` on resource `{resource_type}` has an empty target"
            ),
            Self::InvalidRelationshipStorage {
                resource_type,
                relationship,
                reason,
            } => write!(
                formatter,
                "relationship `{relationship}` on resource `{resource_type}` has invalid storage: {reason}"
            ),
            Self::DuplicateResourceType(type_name) => {
                write!(formatter, "resource type `{type_name}` is duplicated")
            }
            Self::UnknownRelationshipTarget {
                resource_type,
                relationship,
                target_type,
            } => write!(
                formatter,
                "relationship `{relationship}` on resource `{resource_type}` targets unknown type `{target_type}`"
            ),
            Self::UnknownResourceType(type_name) => {
                write!(formatter, "resource type `{type_name}` is not registered")
            }
            Self::UnknownField {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "field `{field_name}` is not registered on resource `{resource_type}`"
            ),
            Self::AttributeNotFilterable {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "attribute `{field_name}` on resource `{resource_type}` is not filterable"
            ),
            Self::AttributeNotSortable {
                resource_type,
                field_name,
            } => write!(
                formatter,
                "attribute `{field_name}` on resource `{resource_type}` is not sortable"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

fn validate_field_name(
    resource_type: &str,
    public_name: &str,
    model_field: &str,
    public_names: &mut HashSet<String>,
    model_fields: &mut HashSet<String>,
) -> Result<(), RegistryError> {
    if public_name.is_empty() {
        return Err(RegistryError::EmptyFieldName {
            resource_type: resource_type.to_owned(),
        });
    }
    if public_name == "id" || public_name == "type" {
        return Err(RegistryError::ReservedFieldName {
            resource_type: resource_type.to_owned(),
            field_name: public_name.to_owned(),
        });
    }
    if !is_valid_member_name(public_name) {
        return Err(RegistryError::InvalidFieldName {
            resource_type: resource_type.to_owned(),
            field_name: public_name.to_owned(),
        });
    }
    if model_field.is_empty() {
        return Err(RegistryError::EmptyModelField {
            resource_type: resource_type.to_owned(),
            field_name: public_name.to_owned(),
        });
    }
    if !public_names.insert(public_name.to_owned()) {
        return Err(RegistryError::DuplicateFieldName {
            resource_type: resource_type.to_owned(),
            field_name: public_name.to_owned(),
        });
    }
    if !model_fields.insert(model_field.to_owned()) {
        return Err(RegistryError::DuplicateModelField {
            resource_type: resource_type.to_owned(),
            model_field: model_field.to_owned(),
        });
    }
    Ok(())
}
