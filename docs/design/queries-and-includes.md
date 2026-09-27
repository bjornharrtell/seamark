# Queries, includes, and limits

**Status: proposal for review.** Query semantics and execution are not implemented.

## Request-to-plan model

Query parameters should be parsed and validated into a typed plan before persistence work begins. The plan should capture the selected resource, fieldsets, include paths, sorting, filters, pagination, and relevant authorization and capability context. Invalid or unsupported combinations should fail explicitly rather than being ignored or approximated.

The contract must account for JSON:API 1.1 fieldsets and supported sorting and pagination behavior. Filtering is not defined as a universal core query language by JSON:API, so any filter parameter and grammar must be documented as an explicit implementation contract, including its supported operators, types, and error behavior.

## Persistence execution

Where supported, query semantics should be translated to database-side work through SeaORM. The design should not silently fall back to in-memory filtering or sorting when a filter or sort cannot be translated. Such a fallback can change pagination results, expose unintended data, or consume unbounded resources.

Requested relationship includes should be loaded in batches or through an otherwise bounded strategy, not by unbounded per-record queries. Fieldsets should inform projection where appropriate, but relationship linkage and other protocol requirements must still be honored. The serializer must distinguish data omitted by a fieldset from data that was requested and found to be absent.

## Resource limits

Applications need configurable limits for at least request size, include depth and breadth, page size, and query complexity. Limits should be applied during planning, before expensive work, and failures should be explicit and testable. Exact defaults and whether limits are global or per-resource remain open.

Authorization must constrain both root and included data. Planning and loading must not let an include path bypass relationship-level access rules.

## Decisions to resolve

- What filter parameter and grammar, if any, should the initial contract support?
- Which filtering and sorting expressions can be guaranteed to execute in the database, and how are unsupported expressions rejected?
- What are the default and maximum pagination and include limits?
- Should applications be able to register custom query operators, and how can those remain safe, typed, and pushable to persistence?
- What query-plan abstraction offers useful testability without leaking SeaORM details into public API types?
