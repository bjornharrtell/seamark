# Resources and SeaORM mapping

**Status: proposal for review.** No resource declaration API or mapping implementation exists yet.

## Public resource schema

A JSON:API resource type is an application-facing contract, not a direct serialization of a database entity. The proposed metadata should describe, at minimum:

- A stable JSON:API `type` and the identifier strategy.
- Exposed attributes and their serialization/deserialization behavior.
- Relationships, their target resource types, cardinality, and permitted operations.
- Read/write permissions and any resource or relationship links the application chooses to expose.
- Validation and authorization integration points.

Applications should opt in to public attributes and relationships. Database columns, internal fields, and ORM relations should not become externally visible merely because they exist in an entity.

## SeaORM integration

SeaORM is the initial persistence foundation. Resource metadata and SeaORM entities should be decoupled enough that applications can use explicit mapping logic, projections, and application-facing types where needed. The design does not require an abstraction over several ORMs.

Reads should favor untracked projections or equivalent read-oriented results where suitable. The mapping boundary should support query plans that select only requested fields when that is compatible with the API contract, and should avoid making persistence-specific model details the public resource definition.

Writes should be explicit. A validated request should map to a request-scoped changeset or command that distinguishes absent fields from fields explicitly set to `null`. This avoids treating an omitted property as an instruction to clear a value and avoids implicit EF-style graph change tracking.

## Relationships and hooks

Relationship metadata should make it possible to:

- Validate relationship identifiers and linkage against registered resource types.
- Load requested relationship data in batches for reads.
- Apply explicit rules for relationship updates and deletion.
- Generate relationship and related-resource links consistently.

Hooks or equivalent extension points may be provided for application validation, authorization, business rules, and custom mapping. Their exact ordering, error model, and transaction context require design. Hooks should not make it possible for framework defaults to silently violate JSON:API semantics.

Derive macros and declarative configuration are possible ergonomics, not settled requirements. Any derive approach should preserve explicit exposure and allow hand-written configuration where generated behavior is insufficient.

## Decisions to resolve

- Is the API resource model an independent type, metadata over SeaORM entities, or a combination with explicit mapping?
- Which identifier formats and key-generation strategies should be supported?
- How should custom scalar serialization, database nullability, and API-level nullability relate?
- What are the syntax and capabilities of declarations, derives, and relationship configuration?
- At what lifecycle points do validation and authorization hooks run, and how can applications override defaults safely?
