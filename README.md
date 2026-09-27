# Seamark

Seamark is an early-stage Rust framework for building JSON:API servers with SeaORM. It currently provides JSON:API document types, an explicit resource registry, a read-only Axum collection/single-resource GET slice, opt-in collection-query and single-resource include/fieldset integration, and prototype SeaORM query and Atomic Operations persistence support. PostgreSQL is the first validated backend; the opt-in `sqlite` feature has partial M7 query and mutation test coverage, but full SQLite parity is not established. Full resource routing and normative specification conformance are not implemented.

## Initial target

The first complete release is intended to support the full normative JSON:API 1.1 base specification and the complete Atomic Operations extension. Support for other third-party extensions and profiles is deferred.

Axum is the initial HTTP integration, with SeaORM and PostgreSQL as the first persistence target. API resources are separate from SeaORM entities and map explicitly to them. Resource definitions are intended to register conventional CRUD and relationship routes by default, with per-route overrides.

Initial query support is deliberately focused: function-style filters, using JsonApiDotNetCore as a reference, will cover equality and null checks with `and`, `or`, and `not`. Filtering applies only to explicitly filterable resource attributes; sortable fields are also explicitly opted in. Repeated filters at the same resource scope combine with OR. Pagination uses a server-defined page-number/page-size contract backed by offset and limit. JSON:API does not define a universal filter or pagination grammar.

## Implementation status

The implementation is tracked in the [implementation plan](docs/implementation-plan.md). The default `router` keeps rejecting non-empty query strings; opt-in `router_with_query` parses and plans supported collection queries and passes the validated plan through a narrow query-adapter boundary after authorization. A PostgreSQL-backed HTTP integration test executes that boundary with the SeaORM query executor and verifies fieldset/included-resource serialization. Atomic Operations has a standalone Axum `POST /operations` router, extension negotiation, mapped changesets, and typed SeaORM mutators for resource CRUD and to-one foreign-key relationships. Applications provide entity value/identifier codecs, route resolution for relationship/resource/collection `href` targets, and custom handlers for to-many relationship persistence. Full normative JSON:API 1.1 and Atomic Operations support remains incomplete; this prototype is not a conformance claim.

## Design documents

- [Architecture and request lifecycle](docs/design/architecture.md)
- [Resources and SeaORM mapping](docs/design/resources-and-mapping.md)
- [Queries, includes, and limits](docs/design/queries-and-includes.md)
- [Mutations and Atomic Operations](docs/design/mutations-and-atomic-operations.md)
- [JSON:API conformance strategy](docs/design/conformance.md)
- [Implementation plan and milestones](docs/implementation-plan.md)

The exact mapping and execution APIs remain open pending further adapter integration. M7 now has partial SQLite implementation evidence; PostgreSQL remains the first validated backend and cross-backend equivalence is incomplete. The design documents describe intent; implementation status and milestone evidence are recorded in the plan.
