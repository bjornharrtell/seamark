# Mutations and Atomic Operations

**Status: partial implementation.** An Atomic Operations planner and
transaction orchestration API are implemented; base mutation routes, concrete
entity changesets, and extension negotiation are not implemented.

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
target types, `ref`/`href` exclusivity, and local-ID ordering. A local ID is
available only after its add operation; relationship linkages can resolve it
through the request-scoped `LocalIdMap`.

`execute_atomic_operations` runs planned operations sequentially in one
SeaORM transaction. Applications supply the operation handler, which performs
the actual resource/relationship writes and returns positional results. A
required guard authorizes the request and applies application limits before
the transaction begins. Handler errors, invalid results, and failed local-ID
assignments roll back preceding writes. PostgreSQL tests exercise ordered
local-ID use and rollback after an injected later-operation failure.

This is an orchestration prototype, not complete extension support. It does
not implement generic entity changesets, an HTTP `POST` route, Atomic
Operations `ext` media-type negotiation, route-aware `href` relationship
operations, or every normative operation/result/error case. The application
handler still owns actual writes and authorization of resources. Exact
changeset, identifier, and hook APIs remain provisional.
