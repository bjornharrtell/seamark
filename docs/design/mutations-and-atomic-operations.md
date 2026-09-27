# Mutations and Atomic Operations

**Status: proposal for review.** Mutation APIs, transaction behavior, and extension support are not implemented.

## Explicit mutation commands

Mutation requests should be parsed and validated into typed commands before persistence work. A command or changeset should be scoped to the request and preserve property presence separately from property value, including an explicit `null`. This makes update semantics reviewable and avoids implicit graph change tracking.

Validation, authorization, and application business rules should be overrideable through defined integration points. The design should specify which checks run before a transaction, which require transactional state, and how failures are represented. Entity updates and relationship changes should be explicit rather than inferred from arbitrary ORM object graphs.

## Transactions and concurrency

Database transaction boundaries should be explicit. The design must specify atomicity across all writes in a request, rollback behavior on validation or persistence failure, and how generated identifiers and server-side defaults affect the response.

Optimistic concurrency remains an explicit design concern. The framework should define how conditional requests and persistence concurrency checks interact, how conflicts map to protocol errors, and how applications can supply versioning policies. It must not imply conflict protection where the persistence adapter cannot provide it.

## Atomic Operations

Atomic Operations is an important supported-extension goal. Its operation document should be validated into an ordered operation plan before execution. The target semantics include ordered execution, all-or-nothing transactional behavior, and local-ID references between operations, including correct resolution and error reporting.

The design must define the supported operation kinds and resource/relationship cases, identifier generation and local-ID scope, transaction isolation expectations, and behavior when the database cannot satisfy the required atomicity. Unsupported operations must fail explicitly; they must not be partially applied or represented as successful.

## Negotiated support

Atomic Operations must be treated as an extension with correct media-type negotiation. The server should advertise and accept it only when the endpoint implements and tests the relevant behavior. Third-party extensions and profiles are not automatically supported; each advertised capability needs its own implementation, negotiation rules, and conformance evidence.

## Decisions to resolve

- What initial scope of Atomic Operations is required, and is every specified operation kind in scope for the first release?
- What are the supported local-ID reference forms and their validation rules?
- What transaction guarantees can be made across supported SeaORM database backends?
- How should optimistic concurrency be configured and surfaced in JSON:API responses?
- Which third-party profiles or extensions, if any, are candidates beyond Atomic Operations?
