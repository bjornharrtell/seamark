# Queries, includes, and limits

**Status: partial implementation.** The adapter-independent parser/read
planner and a focused SeaORM/PostgreSQL collection executor are implemented.
The default Axum router continues to reject query strings. The opt-in
`router_with_query` route parses collection-query parameters and passes a
validated plan to an application-provided query adapter after authorization.
A PostgreSQL-backed integration test connects that adapter to the SeaORM
executor and verifies root and included-resource output.

## Filters, sorting, and pagination

JSON:API does not define a universal filter grammar or pagination contract. Seamark's initial filter syntax uses a function-style form, with JsonApiDotNetCore as a reference. The parser supports `equals(field,'literal')`, `equals(field,null)`, `and`, `or`, and `not`; repeated filters at one scope combine with OR. Apostrophes in string literals are doubled. Range comparisons, text functions, relationship-path filters, `has`, and `count` are outside the initial scope.

Filters apply only to public resource attributes explicitly registered as filterable. Sort fields are likewise explicitly opted in per field. Planning resolves public attribute and relationship names to their registered internal model-field names and rejects unknown operators, fields, relationship paths, malformed values, and unsupported query-parameter names.

Pagination is a server contract using one-based page number and positive page size, translated to offset and limit. `PaginationConfig` requires the application to supply page defaults and any maximum page-size/offset policy; the library invents no defaults or caps. Requested values, multiplication overflow, and configured boundaries are validated before execution.

Because `ReadPlan` and `Page` are publicly constructible, the SeaORM executor
also checks that manually supplied page numbers, sizes, limits, and offsets
form a consistent page before invoking the read guard. It then applies the
guard's explicitly configured limits before authorization and SQL. Negative
values cannot be represented by `Page`'s unsigned fields. This executor check
adds no default page values or maximums.

`ReadQuery` is the decoded input boundary for filters, sort, pagination,
fieldsets, includes, and unsupported parameter names. `plan_read` produces an
adapter-independent `ReadPlan` with ordered sort terms, per-resource sparse
fieldsets, and a merged include tree. Unsupported parameter names are
de-duplicated and sorted in errors for stable reporting.

## Includes and execution

Includes are part of JSON:API. The planner validates nested relationship paths
and merges duplicates. The SeaORM executor accepts an explicit include-loader
hook because relation traversal and authorization rules depend on application
entities; it passes the include tree and fieldsets to that hook and projects
the returned resources to declared fields. The required read guard applies
application-specific page/include limits before authorization and database
work. Applications remain responsible for authorizing included records in
their loader.

Because `ReadPlan` is publicly constructible, the executor recursively
revalidates every include node against the registered relationship's exact
public name, internal mapping, and target resource type before authorization,
the include loader, or SQL. Invalid nodes return
`InvalidIncludeRelationship`, including mismapped nested nodes. Valid nested
include trees remain application-loaded; this validation neither infers ORM
relations nor replaces the loader with in-memory traversal.

Fieldset mappings are checked against the registry again at the executor
boundary because `ReadPlan` is publicly constructible. Invalid attribute or
relationship mappings return `InvalidFieldsetField` before authorization or
database execution. Mapper output is independently intersected with registered
fields before fieldset projection, preserving the registry allowlist even for
direct executor callers.

Filter AST fields are also revalidated against registered filterable
attributes before authorization. A manually constructed plan cannot filter on
a relationship or mapper-only field; valid string literals still pass through
the configured value codec and become SeaORM-bound column comparisons, with no
in-memory filtering fallback.

Sort terms are likewise revalidated against the registered public name,
internal model field, and explicit sortable opt-in before authorization. This
prevents direct callers from using a mapped relationship column or an
otherwise non-sortable entity column to bypass `plan_read`.

The SeaORM executor maps internal field names to the entity's `Column` type and executes filter predicates, ordering, offset, and limit in the configured database backend; it has no in-memory fallback. Its fallible constructor validates the identifier and filterable/sortable attribute columns against the entity before serving requests. The `SeaOrmFilterValueCodec` converts string literals to entity value types and reports conversion failures before querying. A model mapper converts typed rows into internal-field-keyed adapter records, after which the executor enforces sparse-field projections. PostgreSQL integration coverage exercises string equality/OR, null predicates, typed numeric equality, sort, pagination, fieldsets, include loading, and pre-query authorization/limit rejection; the fixture also verifies application-encoded boolean equality alongside typed numeric filters. HTTP integration confirms planned parameters reach this executor and serialized fieldsets/included resources are returned. Relationship loaders and value codecs remain explicit application hooks; broader relation and authorization/resource-limit cases remain incomplete. The opt-in SQLite M7 fixture covers the same supported query categories, including typed numeric/boolean and null filters, ordering, pagination, fieldsets, and application-loaded includes. Both backends construct the same first/second-page queries from `tests/support/query_cases.rs` and assert the same resource/included identities and visible attributes. Full document serialization equivalence, Atomic result equivalence, and broader type coverage remain incomplete.
