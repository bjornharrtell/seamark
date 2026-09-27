# Seamark

Seamark is an early-stage design for a Rust library/framework that aims to provide a structured, flexible JSON:API server experience for applications using SeaORM. Axum is the initial HTTP integration target.

**Status: design only.** No implementation or conformance claim is implied by this repository. The architecture and design documents are proposals subject to review.

## Purpose

The project aims to offer applications a maintainable way to expose JSON:API resources without coupling their public API schema to database tables. The goal is full JSON:API 1.1 compliance, not a selected subset. Correct negotiation and implementation of any advertised extensions and profiles are part of that goal; support must not be inferred merely because a client requests one.

## Design goals

- Use SeaORM as the initial persistence foundation, without requiring a generic multi-ORM abstraction.
- Keep API resource metadata distinct from persistence entities so applications explicitly choose what is exposed.
- Parse and validate requests into typed plans before persistence work; translate supported query behavior to database-side work rather than silently filtering in memory.
- Make mutations explicit through request-scoped, presence-aware changesets and commands. Keep transactions and optimistic concurrency visible design concerns.
- Provide practical resource and relationship declarations, standard JSON:API routing and response handling, validation and authorization hooks, link generation, and a testable Axum integration.
- Preserve room for applications to override framework behavior and apply their own business rules.
- Trace normative protocol requirements to tests through a conformance ledger.

## Design principles

1. The JSON:API contract is explicit and testable; unsupported capabilities are not presented as supported.
2. API schema, application rules, and persistence mapping have clear boundaries.
3. Query cost and resource consumption are bounded, observable, and enforced.
4. Defaults should be useful, while important application decisions remain overrideable.
5. Open design choices are recorded rather than treated as settled.

## Non-goals

- GraphQL or a GraphQL compatibility layer.
- Automatically exposing every database column or relationship.
- Requiring generic support for multiple ORMs in the initial design.
- Claiming that code, routes, protocol behavior, or conformance tests already exist.
- Automatically supporting arbitrary third-party extensions or profiles.

## Design documents

- [Architecture and request lifecycle](docs/design/architecture.md)
- [Resources and SeaORM mapping](docs/design/resources-and-mapping.md)
- [Queries, includes, and limits](docs/design/queries-and-includes.md)
- [Mutations and Atomic Operations](docs/design/mutations-and-atomic-operations.md)
- [JSON:API conformance strategy](docs/design/conformance.md)

## Open questions

The design documents call out unresolved choices, including how resource/API models map to SeaORM entities, what filtering grammar and operator contract to adopt, whether standard routes are registered by default or explicitly, and the initial scope of Atomic Operations and third-party profiles.
