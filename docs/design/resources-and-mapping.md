# Resources and SeaORM mapping

**Status: partial implementation.** An adapter-independent resource registry
and a focused SeaORM collection-execution prototype are implemented. A
finalized production entity/relationship mapping API is not implemented.

## Public resource schema

An API resource is a public JSON:API model, separate from its SeaORM entity, and maps explicitly to that entity. Its registered metadata describes the public `type`, identifier, exposed attributes, relationships, and permitted operations. Database columns and ORM relations are not exposed merely because they exist.

This explicit metadata is also the basis for dynamic request queries. Public fields and relationships map to SeaORM columns and relations through registered mapping information; neither queryability nor sortability is inferred from a resource or entity's structure. Attributes must be explicitly marked filterable to accept filters and explicitly marked sortable to accept sorting.

## Persistence mapping

SeaORM is the initial persistence foundation, with PostgreSQL first. Reads should translate supported plans into database-side selection and relationship loading. Writes should map validated requests to explicit, request-scoped changesets or commands that distinguish an omitted property from one explicitly set to `null`.

The mapping must keep public resource definitions independent of persistence details while making the mapping explicit enough to validate and execute supported queries. The current executor resolves internal field strings through a typed entity's SeaORM `Column` parser and requires an application-provided filter-value encoder and model-to-adapter mapper. Relationship loading is supplied through an explicit loader hook rather than inferred from opaque registry strings. It does not require a generic multi-ORM abstraction or promise alternative resource-definition patterns in the initial release.

## Relationships and application integration

Registered relationship metadata should identify target resource types and support validating linkage, loading requested related data, and applying explicit relationship-update rules. Applications will need integration points for validation, authorization, business rules, and custom mapping; their exact lifecycle and ordering are not settled here.

## Prototype and open choices

A focused PostgreSQL test now verifies typed field resolution, database-side
filtering (including OR, null, and mapped numeric values), sorting, pagination,
identifier serialization, sparse projection, and include loading. This validates
the prototype approach but does not freeze the declaration API. The registry still maps public names to opaque strings and does not
automatically derive SeaORM relationships, identifier codecs, or CRUD
behavior. Atomic Operations plans now map registered public attribute and
relationship names to internal model-field names in request-scoped changesets;
handlers still perform the actual writes. Derive and configuration syntax,
generalized identifier conversion, relation metadata, hook ordering, and
transaction details remain open.
