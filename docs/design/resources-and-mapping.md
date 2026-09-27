# Resources and SeaORM mapping

**Status: partial implementation.** An adapter-independent resource registry
and a focused SeaORM collection-execution prototype are implemented. A
finalized production entity/relationship mapping API is not implemented.

## Public resource schema

An API resource is a public JSON:API model, separate from its SeaORM entity, and maps explicitly to that entity. Its registered metadata describes the public `type`, identifier, exposed attributes, relationships, and permitted operations. Database columns and ORM relations are not exposed merely because they exist.

This explicit metadata is also the basis for dynamic request queries. Public fields and relationships map to SeaORM columns and relations through registered mapping information; neither queryability nor sortability is inferred from a resource or entity's structure. Attributes must be explicitly marked filterable to accept filters and explicitly marked sortable to accept sorting.

The registry rejects resource type, attribute, and relationship names that
violate JSON:API member-name constraints, in addition to rejecting reserved
`id`/`type` fields and duplicate mappings.

## Persistence mapping

SeaORM is the persistence foundation. PostgreSQL is the first validated backend; SQLite is the M7 second backend behind an opt-in Cargo feature, with implementation and parity evidence still partial. Reads should translate supported plans into database-side selection and relationship loading. Writes should map validated requests to explicit, request-scoped changesets or commands that distinguish an omitted property from one explicitly set to `null`.

The mapping must keep public resource definitions independent of persistence details while making the mapping explicit enough to validate and execute supported queries. The current executor resolves internal field strings through a typed entity's SeaORM `Column` parser and requires an application-provided `SeaOrmFilterValueCodec` and model-to-adapter mapper. Typed Atomic mutation handlers use `SeaOrmMutationValueCodec`; applications implementing both can use the combined `SeaOrmValueCodec` contract. Relationship loading is supplied through an explicit loader hook rather than inferred from opaque registry strings. It does not require a generic multi-ORM abstraction or promise alternative resource-definition patterns in the initial release.

`SeaOrmQueryExecutor::new` is fallible and validates the registered
identifier column and every filterable or sortable attribute against the
bound SeaORM entity at construction. Non-queryable attributes may remain
computed mapper outputs. Relationship mapping/loading remains explicit and
application-defined.

At execution, fieldset entries are revalidated against the exact registered
public name, model field, and relationship target. Sort terms are revalidated
against the exact public/internal attribute mapping and its explicit sortable
opt-in. The executor also intersects mapper output with registered attributes
and relationships before applying a fieldset, so a manually constructed read
plan cannot bypass the registry through direct `SeaOrmReadResult` use.

## Relationships and application integration

Registered relationship metadata should identify target resource types and support validating linkage, loading requested related data, and applying explicit relationship-update rules. Applications will need integration points for validation, authorization, business rules, and custom mapping; their exact lifecycle and ordering are not settled here.

## Prototype and open choices

A focused PostgreSQL test now verifies typed field resolution, database-side
filtering (including OR, null, mapped numeric and boolean values), sorting,
pagination, identifier serialization, sparse projection, and include loading.
The PostgreSQL HTTP integration also executes planned collection queries
through Axum and the SeaORM executor. This validates the prototype approach
but does not freeze the declaration API. The registry still maps public names
to opaque strings and does not automatically derive SeaORM relationships or
CRUD behavior; identifier conversion is an explicit mutation-codec hook.
Atomic Operations plans map registered public attribute and
relationship names to internal model-field names in request-scoped changesets.
Typed SeaORM handlers use these changesets for CRUD and to-one foreign-key
writes. `SeaOrmJoinTableMutationHandler` supports to-many add/remove and
Atomic `update` replacement for an explicitly configured two-column
join-table entity, using the mutation codec and shared transaction; the
registry does not contain enough cardinality or join-table metadata to infer
that configuration. Other association shapes still require an application
executor. Derive and configuration syntax,
generalized identifier conversion, relation metadata, hook ordering, and
transaction details remain open. Shared query and mutation codec traits now
provide typed boundaries for their respective executor paths, while mapping
rules and concrete conversions remain application-defined. SQLite fixtures
exercise supported query behavior, enforced foreign-key linkage, typed Atomic
CRUD/relationship updates, and rollback using isolated in-memory databases.
PostgreSQL and SQLite consume the same core port/person fixtures and string,
typed numeric/boolean, and null filter expectations from
`tests/support/query_cases.rs`. CI already runs `--all-features`, so the opt-in
SQLite fixture participates in the existing test job. The shared Atomic fixture
declares a to-one owner foreign-key relation and exercises valid linkage on both
backends; rejection of an orphan foreign key is not separately asserted. The
core query and mutation adapters contain no backend-specific execution branch;
application codecs and relationship hooks remain responsible for type and
relation mapping. No intentional protocol capability difference has been
verified. Full response and Atomic result comparisons, broader
identifier/type coverage, and the remaining cross-backend parity matrix are
still required; the current SQLite tests are not a complete support claim.
