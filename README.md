# Seamark

Seamark is an early-stage design for a Rust framework for building JSON:API servers with SeaORM. **It is not implemented:** this repository makes no claim of working routes, protocol behavior, or conformance.

## Initial target

The first complete release is intended to support the full normative JSON:API 1.1 base specification and the complete Atomic Operations extension. Support for other third-party extensions and profiles is deferred.

Axum is the initial HTTP integration, with SeaORM and PostgreSQL as the first persistence target. API resources are separate from SeaORM entities and map explicitly to them. Resource definitions are intended to register conventional CRUD and relationship routes by default, with per-route overrides.

Initial query support is deliberately focused: function-style filters, using JsonApiDotNetCore as a reference, will cover equality and null checks with `and`, `or`, and `not`. Filtering applies only to explicitly filterable resource attributes; sortable fields are also explicitly opted in. Repeated filters at the same resource scope combine with OR. Pagination uses a server-defined page-number/page-size contract backed by offset and limit. JSON:API does not define a universal filter or pagination grammar.

## Design documents

- [Architecture and request lifecycle](docs/design/architecture.md)
- [Resources and SeaORM mapping](docs/design/resources-and-mapping.md)
- [Queries, includes, and limits](docs/design/queries-and-includes.md)
- [Mutations and Atomic Operations](docs/design/mutations-and-atomic-operations.md)
- [JSON:API conformance strategy](docs/design/conformance.md)

The exact mapping and execution APIs remain open pending a focused prototype. These documents describe design intent, not implemented behavior.
