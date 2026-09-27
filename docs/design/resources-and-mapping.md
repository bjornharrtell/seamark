# Resources and SeaORM mapping

**Status: partial implementation.** An adapter-independent resource registry
prototype is implemented. SeaORM entity mapping and database execution are
not implemented yet.

## Public resource schema

An API resource is a public JSON:API model, separate from its SeaORM entity, and maps explicitly to that entity. Its registered metadata describes the public `type`, identifier, exposed attributes, relationships, and permitted operations. Database columns and ORM relations are not exposed merely because they exist.

This explicit metadata is also the basis for dynamic request queries. Public fields and relationships map to SeaORM columns and relations through registered mapping information; neither queryability nor sortability is inferred from a resource or entity's structure. Attributes must be explicitly marked filterable to accept filters and explicitly marked sortable to accept sorting.

## Persistence mapping

SeaORM is the initial persistence foundation, with PostgreSQL first. Reads should translate supported plans into database-side selection and relationship loading. Writes should map validated requests to explicit, request-scoped changesets or commands that distinguish an omitted property from one explicitly set to `null`.

The mapping must keep public resource definitions independent of persistence details while making the mapping explicit enough to validate and execute supported queries. It does not require a generic multi-ORM abstraction or promise alternative resource-definition patterns in the initial release.

## Relationships and application integration

Registered relationship metadata should identify target resource types and support validating linkage, loading requested related data, and applying explicit relationship-update rules. Applications will need integration points for validation, authorization, business rules, and custom mapping; their exact lifecycle and ordering are not settled here.

## Prototype and open choices

A small resource-to-SeaORM mapping prototype should establish which mappings and dynamic queries are practical before the declaration API is finalized. The current registry is a deliberately provisional first step: it maps public resource, attribute, and relationship names to opaque internal field-name strings; validates relationship targets; and requires explicit attribute filter/sort flags. To avoid ambiguous serialization and query resolution, this initial prototype rejects duplicate internal field mappings, including collisions with the identifier mapping; public aliases over one backing field are not supported. It does not inspect SeaORM entities, resolve database columns, or execute reads or writes. The SeaORM/PostgreSQL milestone must verify whether these mappings are practical before the declaration API is frozen. Derive and configuration syntax, identifier conversion types, exact field and nullability APIs, hook ordering, and transaction details remain undecided until that work is done.
