# Architecture and request lifecycle

**Status: partial implementation.** The repository has a protocol model, an
adapter-independent resource registry and read planner, Axum read-only GET
routes with opt-in collection-query planning, a PostgreSQL-backed SeaORM
collection-executor prototype integrated through the query-adapter boundary,
and a standalone Atomic Operations `POST /operations` router, typed SeaORM
resource mutation executors, and transaction runner. SQLite has an opt-in SeaORM feature and partial M7
query/mutation test coverage; cross-backend parity is not established. Complete JSON:API
conformance is not implemented.

## Boundaries

The design separates protocol handling, public API metadata, application policy, and persistence:

1. **Axum integration** handles HTTP requests and responses. Axum is the initial HTTP target.
2. **Protocol layer** parses and validates JSON:API documents and query parameters, negotiates supported capabilities, and serializes resource, relationship, error, and operation documents.
3. **Resource registry** explicitly describes public resources, fields, relationships, and permitted operations. API resources are separate from SeaORM entities and map to them through registered metadata.
4. **Planning layer** resolves requests into validated read or mutation plans.
5. **Application integration** provides validation, authorization, business rules, and link customization.
6. **SeaORM persistence integration** executes planned database work. PostgreSQL is the first validated target; SQLite is the explicit second target in M7. A generic multi-ORM abstraction is not an initial requirement.

## Routes and request flow

The current read HTTP integration registers only collection and single-resource GET routes. Atomic Operations is exposed separately as a mergeable `POST /operations` router, so applications can combine it with the read routes. Resource definitions are intended to grow into conventional CRUD and relationship routes, with per-route overrides, in later milestones.

The default GET router rejects non-empty query strings; the opt-in query router parses and plans supported collection queries, resolves the resource definition, authorizes the request, then passes the validated plan to a query adapter. The application adapter can execute the plan with the typed SeaORM executor; a PostgreSQL integration test covers this full HTTP-to-database path, including fieldsets and included resources. Application-specific filter-value conversion, authorization/limit checks, model mapping, and included-resource loading remain explicit inputs. Single-resource GET query parameters remain unsupported. Atomic Operations requests are negotiated for the required extension, validated into ordered plans with public fields resolved to internal model fields, and run through one SeaORM transaction. Typed per-entity mutators support resource CRUD and to-one foreign-key writes; applications provide value/identifier codecs and may register custom executors for to-many relations. A route resolver can map relationship `href` references to registered relationship targets. Full normative behavior remains future work.

Capability negotiation must expose only supported behavior; unsupported behavior should fail explicitly rather than being silently approximated.

## Cross-cutting concerns

Error responses, link generation, authorization of included data, and resource-use limits need consistent framework boundaries. Exact hook ordering, public API stability, and implementation-specific mapping and execution choices remain open for a focused prototype.
