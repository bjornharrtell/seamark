# Mutations and Atomic Operations

**Status: partial implementation.** An Atomic Operations planner, mapped
changesets, a standalone Axum route, extension negotiation, typed SeaORM
resource CRUD, to-one foreign-key writes, nullable direct-FK to-many
add/remove/replacement, explicitly configured two-column join-table membership writes,
and transaction orchestration are implemented. Other to-many association
shapes and full normative conformance remain application-specific or
incomplete.

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
mapped relationship field. Unrecognized members are ignored as required by
JSON:API processing rules, while the extension-forbidden top-level `data` and
`included` members are explicitly rejected. A local ID is available only after
its add operation; relationship linkages can resolve it through the
request-scoped `LocalIdMap`.

`atomic_http::router` provides a mergeable `POST /operations` Axum router;
`router_with_href_resolver` additionally accepts an application route resolver
for relationship, resource, and collection `href` targets. Resolved targets
are registry-validated before transaction start: resource references become
normal resource targets, and collection references must match the added
resource's type. Unresolved targets remain available to custom handlers. Both
routers require the quoted Atomic Operations extension in `Content-Type` and
`Accept`, reject query parameters, validate request documents, and emit
JSON:API result/error documents with `Vary: Accept`.

`SeaOrmResourceMutationHandler<E, C>` maps a public resource to one typed
SeaORM entity and performs add/update/remove plus to-one relationship FK
updates. Applications supply explicit value and identifier codecs. A
`SeaOrmAtomicOperationDispatcher` composes typed handlers and application
executors. The typed handler supports route-resolved resource `href` updates
and deletes and collection `href` adds after the planner normalizes or
validates them. Relationship `href` targets are normalized to
registry-checked references. To-many resource changesets and unsupported
relationship association shapes remain available for custom handlers or the
dedicated typed relationship executors.

`SeaOrmJoinTableMutationHandler<E, C>` handles to-many add/remove operations
and full membership replacement through Atomic `update` for a configured
relationship backed by a typed two-column join-table entity.
The application explicitly supplies the source resource, public relationship,
join entity columns, and mutation codec; the helper resolves persistent and
request-local identifiers and uses the operation's shared transaction. It does
not infer cardinality or join-table structure from the registry's opaque
relationship field. `SeaOrmToManyForeignKeyMutationHandler<E, C>` supports
to-many add/remove for a relationship represented by a nullable FK column on
the related entity. It takes the source resource and relationship from the
registry plus the target FK column explicitly. Add assigns an unowned target
or accepts one already owned by the source; it rejects implicit reassignment
from another source. Remove clears the FK only for targets currently owned by
that source. It does not implement replacement, infer FK nullability, or
preserve member ordering. Join tables with additional required columns,
ordered relationships, non-nullable direct FKs, and other custom persistence
rules continue to use application-provided
`SeaOrmAtomicOperationExecutor` implementations.
The two-column mapping has no position column and does not preserve linkage
order; applications that define ordered relationships must provide a custom
executor.

`execute_atomic_operations` runs planned operations sequentially in one
SeaORM transaction. A required guard authorizes the request and applies
application limits before the transaction begins. Handler errors, invalid
results, and failed local-ID assignments roll back preceding writes.
PostgreSQL tests exercise ordered local-ID use, typed create/update/delete,
to-one linkage, resolved relationship hrefs, and rollback after a later
operation fails. HTTP tests cover negotiation, success, and error paths.

This is a partial persistence integration, not complete extension support.
Applications still define each resource's entity, value/identifier codecs,
route mappings, resource-level authorization, and persistence for association
shapes outside the configured join-table and nullable direct-FK helpers. It
does not implement every normative operation/result/error case. Exact
changeset and hook APIs remain provisional.
