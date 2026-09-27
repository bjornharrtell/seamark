# Mutations and Atomic Operations

**Status: proposal for review.** Mutation APIs, transaction behavior, and extension support are not implemented.

## Mutations and concurrency

Mutation requests should be validated into explicit commands or changesets before writes. Changesets preserve whether a property was omitted or explicitly set, including to `null`, so updates do not infer changes from arbitrary ORM object graphs.

Optimistic concurrency is application-defined initially. Built-in version, ETag, or If-Match support may be revisited later; the initial design does not prescribe framework-managed conflict behavior.

## Atomic Operations

The complete JSON:API Atomic Operations extension is a target for the first complete release. Operations must execute in document order within a transaction, support local-ID (`lid`) references between operations, and commit as a unit. A failure must not leave partial writes or be represented as success.

Atomic Operations requires correct extension negotiation as well as complete operation behavior. Other third-party extensions and profiles are deferred beyond the initial target.

## Prototype

A focused mapping and execution prototype will inform the exact changeset, operation-plan, identifier-mapping, and transaction APIs. Those implementation details, along with hook ordering, are not frozen by this design.
