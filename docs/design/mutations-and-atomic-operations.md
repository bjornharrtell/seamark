# Mutations and Atomic Operations

**Status: partial implementation.** An Atomic Operations planner, mapped
changesets, a standalone Axum route, extension negotiation, typed SeaORM
resource CRUD, to-one foreign-key writes, and transaction orchestration are
implemented. To-many persistence and full normative conformance remain
application-specific or incomplete.

## Mutations and concurrency

Mutation requests should be validated into explicit commands or changesets before writes. Changesets preserve whether a property was omitted or explicitly set, including to `null`, so updates do not infer changes from arbitrary ORM object graphs.

Optimistic concurrency is application-defined initially. Built-in version, ETag, or If-Match support may be revisited later; the initial design does not prescribe framework-managed conflict behavior.

## Atomic Operations

The complete JSON:API Atomic Operations extension is a target for the first complete release. Operations must execute in document order within a transaction, support local-ID (`lid`) references between operations, and commit as a unit. A failure must not leave partial writes or be represented as success.

Atomic Operations requires correct extension negotiation as well as complete operation behavior. Other third-party extensions and profiles are deferred beyond the initial target.

## Current Atomic Operations prototype

`AtomicOperationsDocument` models `atomic:operations` and `atomic:results`
separately from base primary data. The planner validates add/update/remove
shapes, registry resource/attribute/relationship mappings, relationship
target types, `ref`/`href` exclusivity, and local-ID ordering. Add/update
operations carry mapped changesets: attributes and relationships are keyed by
their configured internal model fields, and omitted properties remain
distinct from explicit `null` values. Relationship operations also carry the
mapped relationship field. A local ID is available only after its add
operation; relationship linkages can resolve it through the request-scoped
`LocalIdMap`.

`atomic_http::router` provides a mergeable `POST /operations` Axum router;
`router_with_href_resolver` additionally accepts an application route resolver
for relationship `href` targets. Both require the quoted Atomic Operations
extension in `Content-Type` and `Accept`, reject query parameters, validate
request documents, and emit JSON:API result/error documents with
`Vary: Accept`.

`SeaOrmResourceMutationHandler<E, F, I>` maps a public resource to one typed
SeaORM entity and performs add/update/remove plus to-one relationship FK
updates. Applications supply explicit value and identifier codecs. A
`SeaOrmAtomicOperationDispatcher` composes typed handlers and application
executors; unsupported to-many relations and `href` resource targets remain
available for custom handlers. The typed handler supports route-resolved
relationship hrefs after the planner normalizes them to registry-checked
references.

`execute_atomic_operations` runs planned operations sequentially in one
SeaORM transaction. A required guard authorizes the request and applies
application limits before the transaction begins. Handler errors, invalid
results, and failed local-ID assignments roll back preceding writes.
PostgreSQL tests exercise ordered local-ID use, typed create/update/delete,
to-one linkage, resolved relationship hrefs, and rollback after a later
operation fails. HTTP tests cover negotiation, success, and error paths.

This is a partial persistence integration, not complete extension support.
Applications still define each resource's entity, value/identifier codecs,
relationship route resolver, resource-level authorization, and custom
to-many persistence. It does not implement every normative
operation/result/error case. Exact changeset and hook APIs remain provisional.
