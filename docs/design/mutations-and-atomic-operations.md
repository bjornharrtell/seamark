# Mutations and Atomic Operations

**Status: partial implementation.** Base Axum resource and relationship-linkage
routes use explicit application mutation adapters; `SeaOrmBaseMutationAdapter`
provides one transaction per typed command. An Atomic Operations planner,
mapped changesets, a standalone Axum route, extension negotiation, typed SeaORM
resource CRUD, to-one foreign-key writes, nullable direct-FK to-many
add/remove/replacement, explicitly configured two-column join-table membership writes,
resource-level to-many adds and updates composed through typed relationship handlers,
and transaction orchestration are implemented. Other to-many association
shapes and full normative conformance remain application-specific or
incomplete.

## Mutations and concurrency

Mutation requests should be validated into explicit commands or changesets before writes. Changesets preserve whether a property was omitted or explicitly set, including to `null`, so updates do not infer changes from arbitrary ORM object graphs.

Optimistic concurrency is application-defined initially. Built-in version, ETag, or If-Match support may be revisited later; the initial design does not prescribe framework-managed conflict behavior.

## Base HTTP resource and relationship operations

`http::router_with_mutations` adds ordinary JSON:API HTTP methods to the
existing collection and resource GET routes. Its canonical paths are
`POST /{type}`, `PATCH|DELETE /{type}/{id}`, and
`GET|PATCH|POST|DELETE /{type}/{id}/relationships/{name}`. It deliberately
does not register related-resource GET URLs. `router_with_query_and_mutations`
composes these routes with the opt-in query router. The default `router`
remains GET-only.

`ResourceDefinition::to_one_relationship` and
`ResourceDefinition::to_many_relationship` declare the cardinality needed to
validate relationship linkage and method shape. Existing
`ResourceDefinition::relationship` declarations remain cardinality-unspecified;
they can be used for reads but mutation endpoints reject writes until
cardinality is declared. Applications can use their own Axum routes with the
read-only router to choose alternative URL shapes; the framework does not
provide a path-template DSL.

Resource POST accepts a primary resource object whose `type` matches the
collection. The server assigns its identity: client-supplied `id` is not
supported and returns 403. A successful create returns 201 with primary
resource data and a `Location` header. Resource PATCH requires matching `type`
and `id` and only changes submitted fields. Omitted attributes and
relationships remain unchanged; an explicit attribute `null` or relationship
`data: null` remains a distinct requested change. Every supplied relationship
must contain `data`. Resource DELETE returns 204 after successful deletion.
Create type mismatch and PATCH type/ID mismatch return 409; nonexistent
resources or related targets return 404 through the adapter error mapping.
These selected responses follow the JSON:API 1.1 [resource creation](https://jsonapi.org/format/1.1/#crud-creating),
[resource update](https://jsonapi.org/format/1.1/#crud-updating), and
[resource deletion](https://jsonapi.org/format/1.1/#crud-deleting) rules.

Relationship GET returns `200` with resource linkage as primary `data`. PATCH
replaces to-one or to-many linkage; POST and DELETE accept to-many linkage
arrays only. Their `data` arrays may be empty. A successful write returns 200
with the resulting linkage. Application executors must make POST idempotent
(already-linked identifiers are not duplicated) and DELETE idempotent
(already-absent linkage members are successful no-ops). Invalid method and
cardinality combinations return 403. Unknown attributes/relationships,
malformed documents, and unsupported media types are rejected before
authorization or adapter execution; authorization denial also precedes any
adapter call. These semantics and the 200/204 response alternatives follow
the JSON:API 1.1 [fetching relationships](https://jsonapi.org/format/1.1/#fetching-relationships)
and [updating to-many relationships](https://jsonapi.org/format/1.1/#crud-updating-to-many-relationships)
rules.

`MutationCommand`, `ResourceMutationChangeset`, and
`MutationResourceAdapter` form the base HTTP persistence contract. The route
layer resolves public names through the registry but does not translate
commands into Atomic `PlannedOperation`s or use Atomic result documents.
`SeaOrmBaseMutationAdapter` starts one `DatabaseTransaction` per command,
dispatches to the first matching `SeaOrmBaseMutationExecutor`, and commits or
rolls back that command. Applications provide typed executors for their
entities and relationship shapes. These are application hooks, not a generic
automatic mapping from registry strings to database relationships.

The focused PostgreSQL/SQLite route cases in
`tests/support/http_mutation_cases.rs` verify create/update/delete persistence,
to-one linkage, to-many add/replace/remove, duplicate additions, empty arrays,
absent-member removal, omitted-versus-null PATCH fields, missing parent/target
404s, and rollback after an attribute write is followed by a missing related
target. The cases use an explicit typed fixture executor to exercise the
adapter contract; they do not claim that arbitrary application entities or
association shapes are inferred automatically.

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
registry-checked references. When no custom executor handles a resource
`update` containing to-many relationship data, the dispatcher executes the
resource's attributes and to-one changes with its typed resource handler, then
dispatches each to-many replacement to its matching typed relationship
executor in the same transaction. The same composition applies to resource
`add`: the typed resource handler creates the source first, then the dispatcher
uses its persistent identity to apply each to-many replacement. If the add
does not declare a local ID, the identity is read from the typed add result.
A failure in either part rolls back the whole operation batch. Custom
executors that directly support the original operation retain first-match
precedence. Unsupported association shapes remain available to custom handlers
or dedicated typed relationship executors.

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
