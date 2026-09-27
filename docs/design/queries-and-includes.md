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
the returned resources to declared fields. The required read guard authorizes
the plan and applies application-specific page/include limits before database
work. Applications remain responsible for authorizing included records in
their loader.

The SeaORM executor maps internal field names to the entity's `Column` type and executes filter predicates, ordering, offset, and limit in PostgreSQL; it has no in-memory fallback. An explicit filter-value encoder converts string literals to the entity's database value types and reports conversion failures before querying. A model mapper converts typed rows into internal-field-keyed adapter records, after which the executor enforces sparse-field projections. PostgreSQL integration coverage exercises string equality/OR, null predicates, typed numeric equality, sort, pagination, fieldsets, include loading, and pre-query authorization/limit rejection; the fixture also verifies application-encoded boolean equality alongside typed numeric filters. HTTP integration confirms planned parameters reach this executor and serialized fieldsets/included resources are returned. The reusable production mapping API and broader unsupported-query/resource-limit matrix remain incomplete. SQLite is planned for M7 and is not covered by this milestone.
