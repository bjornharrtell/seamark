# Architecture and request lifecycle

**Status: proposal for review.** Seamark is not implemented; this page describes intended boundaries and flow.

## Boundaries

The design separates protocol handling, public API metadata, application policy, and persistence:

1. **Axum integration** handles HTTP requests and responses. Axum is the initial HTTP target.
2. **Protocol layer** parses and validates JSON:API documents and query parameters, negotiates supported capabilities, and serializes resource, relationship, error, and operation documents.
3. **Resource registry** explicitly describes public resources, fields, relationships, and permitted operations. API resources are separate from SeaORM entities and map to them through registered metadata.
4. **Planning layer** resolves requests into validated read or mutation plans.
5. **Application integration** provides validation, authorization, business rules, and link customization.
6. **SeaORM persistence integration** executes planned database work. PostgreSQL is the first database target; other SeaORM SQL databases may follow. A generic multi-ORM abstraction is not an initial requirement.

## Routes and request flow

Resource definitions are intended to register conventional CRUD and relationship routes by default. Applications can override individual routes.

A request is matched to a route, negotiated against the endpoint's supported JSON:API capabilities, parsed and validated, and resolved using registered resource metadata. The framework then authorizes and plans the requested work, executes it through SeaORM where persistence is needed, and serializes a protocol response. Mutation requests use explicit changesets or operation plans.

Capability negotiation must expose only supported behavior; unsupported behavior should fail explicitly rather than being silently approximated.

## Cross-cutting concerns

Error responses, link generation, authorization of included data, and resource-use limits need consistent framework boundaries. Exact hook ordering, public API stability, and implementation-specific mapping and execution choices remain open for a focused prototype.
