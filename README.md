# Seamark

Seamark is an early-stage Rust framework for building JSON:API servers with SeaORM. It currently provides JSON:API document types, an explicit resource registry, and a read-only Axum collection/single-resource GET slice. Persistence and full specification conformance are not implemented.

## Initial target

The first complete release is intended to support the full normative JSON:API 1.1 base specification and the complete Atomic Operations extension. Support for other third-party extensions and profiles is deferred.

Axum is the initial HTTP integration, with SeaORM and PostgreSQL as the first persistence target. API resources are separate from SeaORM entities and map explicitly to them. Resource definitions are intended to register conventional CRUD and relationship routes by default, with per-route overrides.

Initial query support is deliberately focused: function-style filters, using JsonApiDotNetCore as a reference, will cover equality and null checks with `and`, `or`, and `not`. Filtering applies only to explicitly filterable resource attributes; sortable fields are also explicitly opted in. Repeated filters at the same resource scope combine with OR. Pagination uses a server-defined page-number/page-size contract backed by offset and limit. JSON:API does not define a universal filter or pagination grammar.

## Implementation status

The implementation is tracked in the [implementation plan](docs/implementation-plan.md). The current HTTP slice uses a narrow adapter boundary, authorizes before adapter calls, negotiates JSON:API responses, and projects only registered fields. It rejects every non-empty query string. SeaORM persistence, general query support, mutations, and full JSON:API 1.1 base-specification and Atomic Operations support remain unimplemented; the current code must not be treated as a conformance claim.

## Design documents

- [Architecture and request lifecycle](docs/design/architecture.md)
- [Resources and SeaORM mapping](docs/design/resources-and-mapping.md)
- [Queries, includes, and limits](docs/design/queries-and-includes.md)
- [Mutations and Atomic Operations](docs/design/mutations-and-atomic-operations.md)
- [JSON:API conformance strategy](docs/design/conformance.md)
- [Implementation plan and milestones](docs/implementation-plan.md)

The exact mapping and execution APIs remain open pending a focused prototype. The design documents describe intent; implementation status and milestone evidence are recorded in the plan.
