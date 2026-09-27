# Architecture and request lifecycle

**Status: partial implementation.** The repository has a protocol model, an
adapter-independent resource registry and read planner, Axum read-only GET
routes with opt-in collection and single-resource query planning, a
PostgreSQL-backed SeaORM executor prototype integrated through the
query-adapter boundary,
and a standalone Atomic Operations `POST /operations` router, opt-in base
resource/relationship routes, typed SeaORM mutation executors, and transaction
runners for both mutation contracts. SQLite has an opt-in SeaORM feature and partial M7
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

The default HTTP integration registers collection and single-resource GET
routes only. `router_with_mutations` and
`router_with_query_and_mutations` opt into the conventional resource and
relationship-linkage methods: `POST /{type}`, `PATCH|DELETE /{type}/{id}`, and
`GET|PATCH|POST|DELETE /{type}/{id}/relationships/{name}`. Related-resource
URLs are not registered. Relationship writes require explicit
`to_one_relationship` or `to_many_relationship` registry declarations;
`.relationship(...)` remains cardinality-unspecified and is suitable for
read-only mapping. Applications that need different paths can compose
application-owned Axum mutation routes with the read-only router. The helpers
do not add a route-template language or silently remap paths.

Base requests are validated and mapped to `MutationCommand` values before
authorization and adapter execution. `MutationResourceAdapter` is distinct
from the Atomic planner, handlers, and result document. Its command changesets
key only supplied fields by internal registry mapping, so omitted PATCH fields
remain absent and explicit `null` remains a value. `SeaOrmBaseMutationAdapter`
dispatches to the first matching typed base executor in one transaction per
command. The typed executor and application are responsible for related-target
existence, idempotent to-many membership updates, model-specific result
representations, and association persistence. Atomic operations continue to
use their own request plan and transaction batch.

The default GET router rejects non-empty query strings. The opt-in query router
parses and plans collection queries plus single-resource includes and sparse
fieldsets, resolves the resource definition, authorizes the request, then
passes the validated plan to a query adapter. Filters, sorting, and pagination
remain collection-only. The typed SeaORM executor handles both plan shapes;
PostgreSQL and SQLite integration tests cover included resources, linkage,
fieldset projection, and unsupported single-resource parameters. Application-
specific filter/identifier conversion, authorization/limit checks, model
mapping, and included-resource loading remain explicit inputs. Atomic
Operations requests are negotiated for the required extension, validated into
ordered plans with public fields resolved to internal model fields, and run
through one SeaORM transaction. Typed per-entity mutators support resource CRUD
and to-one foreign-key writes; applications provide value/identifier codecs
and may register custom executors for to-many relations. A route resolver can
map relationship `href` references to registered relationship targets. Full
normative behavior remains future work.

Capability negotiation must expose only supported behavior; unsupported behavior should fail explicitly rather than being silently approximated.

## Cross-cutting concerns

Error responses, link generation, authorization of included data, and resource-use limits need consistent framework boundaries. Exact hook ordering, public API stability, and implementation-specific mapping and execution choices remain open for a focused prototype.
