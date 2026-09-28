# Queries, includes, and limits

**Status: partial implementation.** The adapter-independent read planner,
composable query routes, and standard SeaORM query adapter support collection
queries and single-resource reads. Collections support filters, sorting,
pagination, includes, and sparse fieldsets. Single-resource reads support
includes and sparse fieldsets; filters, sorting, and pagination are
collection-only.

## Filters, sorting, and pagination

JSON:API does not define a universal filter grammar or pagination contract.
Seamark's initial filter syntax uses `equals(field,'literal')`,
`equals(field,null)`, and the `and`, `or`, and `not` operators. Repeated filters
at one scope combine with OR. Apostrophes in string literals are doubled.
Range comparisons, text functions, relationship-path filters, `has`, and
`count` are outside the current scope.

Filters and sort expressions apply only to explicitly registered attributes
with the corresponding `AttributePermission`. Planning resolves public names
to their registered internal fields and rejects unknown operators, fields,
relationship paths, malformed values, and unsupported query parameters.
Filter expressions and include paths have fixed structural depth caps
(`MAX_FILTER_DEPTH` and `MAX_INCLUDE_DEPTH`) so malformed or hostile input is
rejected before it can drive unbounded recursion. `ExecutionLimits` can still
impose smaller application limits.

Pagination uses a one-based page number and positive page size, translated to
offset and limit. `PaginationConfig` requires the application to provide page
defaults and any maximum page-size or offset policy; Seamark supplies no
implicit caps. Requested values, multiplication overflow, and configured
boundaries are validated before execution. Because `ReadPlan` and `Page` are
publicly constructible, the SeaORM executor revalidates page consistency at
the execution boundary.

`ReadQuery` is the decoded input boundary for filters, sorting, pagination,
fieldsets, includes, and unsupported parameter names. `plan_read` produces an
adapter-independent `ReadPlan` with ordered sort terms, per-resource sparse
fieldsets, and a merged include tree. Unsupported parameter names are
de-duplicated and sorted in errors for stable reporting.

## Includes and projection

The planner validates nested relationship paths and merges duplicate include
paths. Before authorization, custom loaders, or SQL, the SeaORM executor
revalidates each include node against the registry's exact public name,
internal mapping, and target type. Fieldset mappings and filter/sort fields
receive the same execution-boundary validation because callers can construct
plans directly.

The standard SeaORM loader batches the registered to-one foreign-key,
to-many foreign-key, and two-column join-table shapes. Join tables are
registered with a typed SeaORM entity. Nested includes reuse the same registry
and projection rules. Custom loaders remain available for application-defined
association shapes. A relationship declaration does not enable include
access; `RelationshipPermission::Include` must be granted explicitly, and
linkage visibility uses its own permission.

The HTTP `RequestAuthorizer` receives the full validated query plan, including
its include tree, so application policy can authorize root fields and related
resource paths together. `SharedAuthorization` can reuse the same policy for
ordinary mutations and Atomic Operations. `SeaOrmReadGuard` is a lower-level
query hook without HTTP headers and can add query-specific checks.

Projection intersects mapper output with declared fields before applying a
sparse fieldset. A mapper cannot expose an undeclared attribute or
relationship. Read-only computed attributes can be registered with
`computed_attribute_mapping` and mapped from the entity model; they cannot be
filtered or sorted because they do not correspond to database columns.

## Limits and execution

`ExecutionLimits` composes common limits with application checks for include
depth and breadth, filter complexity, relationship member counts, and Atomic
batch size. It can also set runtime maximums for included resources and include
queries. Standard SeaORM loading applies those budgets while querying and
expanding relationships. Custom SeaORM loaders receive the same per-request
budget and can consume row budget before accepting results and query budget
before database calls. Other query adapters are checked against the returned
included-resource count.

The SeaORM executor converts filter literals through the configured typed
codec and builds database-bound column predicates. Sorting, offset, and limit
are also executed by the database; there is no in-memory filtering or sorting
fallback. Unsupported conversions and mismapped plan fields fail explicitly.

Single-resource `GET` uses the same validated `ReadPlan` model for includes and
sparse fieldsets, then dispatches to the query adapter's resource operation.
`plan_resource_read` rejects filters, sorting, and pagination before
authorization or adapter execution. A router built without `ApiBuilder::queries`
continues to reject non-empty query strings.

SQLite integration tests exercise the standard foreign-key and join-table
include paths, sparse fieldsets, filters, sorting, pagination, authorization,
limits, and single-resource reads. PostgreSQL integration tests use the same
shared query fixtures and require `SEAMARK_TEST_DATABASE_URL`.
