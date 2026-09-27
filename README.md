# Seamark

Seamark is an early-stage Rust framework for building JSON:API servers with SeaORM. It currently provides JSON:API document types, an explicit resource registry, Axum collection/single-resource GET routes, opt-in base resource and relationship-linkage mutations, collection-query and single-resource include/fieldset integration, and prototype SeaORM query and Atomic Operations persistence support. PostgreSQL and the opt-in `sqlite` feature have shared base-mutation route/persistence tests, but full SQLite parity is not established. Full normative specification conformance is not implemented.

## Initial target

The first complete release is intended to support the full normative JSON:API 1.1 base specification and the complete Atomic Operations extension. Support for other third-party extensions and profiles is deferred.

Axum is the initial HTTP integration, with SeaORM and PostgreSQL as the first persistence target. API resources are separate from SeaORM entities and map explicitly to them. Mutation-enabled routers use conventional resource and relationship-linkage paths; explicit to-one/to-many registry declarations are required for relationship writes. Applications needing different URL shapes can compose their own Axum mutation routes with the read-only router instead of using a path-template DSL.

Initial query support is deliberately focused: function-style filters, using JsonApiDotNetCore as a reference, will cover equality and null checks with `and`, `or`, and `not`. Filtering applies only to explicitly filterable resource attributes; sortable fields are also explicitly opted in. Repeated filters at the same resource scope combine with OR. Pagination uses a server-defined page-number/page-size contract backed by offset and limit. JSON:API does not define a universal filter or pagination grammar.

## Implementation status

The implementation is tracked in the [implementation plan](docs/implementation-plan.md). `router` remains GET-only; `router_with_mutations` adds `POST /{type}`, `PATCH|DELETE /{type}/{id}`, and `GET|PATCH|POST|DELETE /{type}/{id}/relationships/{name}` using an application mutation adapter. `SeaOrmBaseMutationAdapter` runs each validated command in a transaction through explicitly registered typed executors, independently of Atomic Operations. Client-assigned resource IDs are not supported by the base create route (HTTP 403); server-generated IDs return HTTP 201, a resource representation, and a `Location` header. Resource updates preserve omitted fields, and relationship methods require declared cardinality. The default `router` keeps rejecting non-empty query strings; opt-in `router_with_query` supports the documented collection query grammar and single-resource `include` and `fields[resource-type]` parameters. Full normative JSON:API 1.1 and Atomic Operations support remains incomplete; this prototype is not a conformance claim.

## Design documents

- [Architecture and request lifecycle](docs/design/architecture.md)
- [Resources and SeaORM mapping](docs/design/resources-and-mapping.md)
- [Queries, includes, and limits](docs/design/queries-and-includes.md)
- [Mutations and Atomic Operations](docs/design/mutations-and-atomic-operations.md)
- [JSON:API conformance strategy](docs/design/conformance.md)
- [Implementation plan and milestones](docs/implementation-plan.md)

The exact mapping and execution APIs remain open pending further adapter integration. M7 now has partial SQLite implementation evidence; PostgreSQL remains the first validated backend and cross-backend equivalence is incomplete. The design documents describe intent; implementation status and milestone evidence are recorded in the plan.
