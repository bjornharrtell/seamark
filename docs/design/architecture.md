# Architecture and request lifecycle

**Status: partial implementation.** The repository has a protocol model, an
adapter-independent resource registry and read planner, an Axum read-only GET
slice, a focused SeaORM/PostgreSQL collection-executor prototype, and a
standalone Atomic Operations `POST /operations` router and transaction runner.
The read executor is not wired into GET routes, and Atomic Operations still
requires application-supplied entity writes; complete JSON:API conformance is
not implemented.

## Boundaries

The design separates protocol handling, public API metadata, application policy, and persistence:

1. **Axum integration** handles HTTP requests and responses. Axum is the initial HTTP target.
2. **Protocol layer** parses and validates JSON:API documents and query parameters, negotiates supported capabilities, and serializes resource, relationship, error, and operation documents.
3. **Resource registry** explicitly describes public resources, fields, relationships, and permitted operations. API resources are separate from SeaORM entities and map to them through registered metadata.
4. **Planning layer** resolves requests into validated read or mutation plans.
5. **Application integration** provides validation, authorization, business rules, and link customization.
6. **SeaORM persistence integration** executes planned database work. PostgreSQL is the first database target; other SeaORM SQL databases may follow. A generic multi-ORM abstraction is not an initial requirement.

## Routes and request flow

The current read HTTP integration registers only collection and single-resource GET routes. Atomic Operations is exposed separately as a mergeable `POST /operations` router, so applications can combine it with the read routes. Resource definitions are intended to grow into conventional CRUD and relationship routes, with per-route overrides, in later milestones.

A current GET request is negotiated as JSON:API, rejected if it contains query parameters, resolved against the resource registry, and authorized before the read adapter is called. The adapter receives the public resource definition and returns normalized resource records; only explicitly declared fields are serialized. The adapter is persistence-independent. Separately, callers can validate query parameters into a read plan and execute supported collection predicates, ordering, and pagination through a typed SeaORM executor. Application-specific filter-value conversion, authorization/limit checks, model mapping, and included-resource loading are explicit executor inputs. Atomic Operations requests are negotiated for the required extension, validated into ordered plans with public fields resolved to internal model fields, and run through one SeaORM transaction. Applications still supply the operation handler that performs concrete entity and relationship writes. Route-aware `href` relationship resolution and general CRUD handlers remain future work.

Capability negotiation must expose only supported behavior; unsupported behavior should fail explicitly rather than being silently approximated.

## Cross-cutting concerns

Error responses, link generation, authorization of included data, and resource-use limits need consistent framework boundaries. Exact hook ordering, public API stability, and implementation-specific mapping and execution choices remain open for a focused prototype.
