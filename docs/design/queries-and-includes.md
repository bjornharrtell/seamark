# Queries, includes, and limits

**Status: proposal for review.** Query parsing and execution are not implemented.

## Filters, sorting, and pagination

JSON:API does not define a universal filter grammar or pagination contract. Seamark's initial filter syntax will use a function-style form, with JsonApiDotNetCore as a reference. The initial operators are limited to equality and null checks, composed with `and`, `or`, and `not`. Range comparisons, text functions, relationship-path filters, `has`, and `count` are outside the initial scope.

Filters apply only to public resource attributes explicitly registered as filterable. Repeated filters at the same resource scope combine with OR, following JsonApiDotNetCore behavior. Sort fields are likewise explicitly opted in per field.

Pagination is a server contract using page number and page size, translated to offset and limit. Exact limits and defaults are not settled here. Requests should be parsed and validated into a query plan; unsupported or invalid behavior should fail explicitly rather than be ignored or approximated.

## Includes and execution

Includes are part of JSON:API. Requested relationship data should be loaded efficiently, such as in batches, and bounded by application-configurable resource limits. Authorization applies to included data as well as root resources. Fieldsets may inform projections, while the response must still distinguish omitted fields from requested-but-absent data.

Where supported, query semantics should be executed by the database through SeaORM rather than silently falling back to in-memory filtering or sorting. Mapping metadata, not DTO or entity structure, determines which public fields can be queried. Exact mapping and execution APIs, including whether custom operators can be added, will be informed by a focused prototype.
