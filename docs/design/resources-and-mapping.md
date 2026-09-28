# Resources and SeaORM mapping

**Status: partial implementation.** Seamark has an adapter-independent
resource registry, named permissions, typed SeaORM field helpers, standard
query and mutation adapters, common scalar codecs, batched loading for common
relationship shapes, and shared resource projection. This document describes
the implemented mapping boundaries; it is not a claim of complete JSON:API or
Atomic Operations conformance.

## Public resource schema

An API resource is a public JSON:API model, separate from its SeaORM entity.
The registry declares its public type, identifier, attributes, relationships,
and enabled operations. Database columns and ORM relationships are not exposed
merely because they exist.

`attribute_mapping::<Entity>(public_name, Entity::Column::...)` binds an
attribute to a typed SeaORM column. Filtering, sorting, and each write
operation are separate `AttributePermission` values. Standard SeaORM codecs
cover supported scalar, identifier, nullable, date/time, decimal, JSON, and
UUID values; applications can supply custom codecs.

Computed attributes use `computed_attribute_mapping` with a registered
`AttributeMapping` and an application function from the SeaORM model to a JSON
value. The function is shared by the standard query executor and the base
mutation handler when each is configured with that computed mapping. Computed
attributes must be read-only and cannot be filtered or sorted because they do
not correspond to database columns.

The registry rejects invalid or duplicate public names and validates
relationship targets and storage declarations. Startup validation checks
executor coverage, conflicting registrations, entity columns, and supported
relationship metadata before the HTTP router is served.

## Relationship mappings

Relationship metadata declares target resource type, cardinality, storage,
nullability where applicable, reassignment policy, and operation permissions.
Typed helpers bind source foreign-key columns, target-side foreign-key columns,
and join-table columns to generated SeaORM column enums.

The standard query adapter batches to-one foreign-key lookups and to-many
foreign-key lookups. Join-table relationships use an explicitly registered
typed join entity. Nested includes use the same registry and projection rules.
Nonstandard association shapes can use custom include loaders and mutation
executors. Declaring a relationship does not enable includes or writes by
itself.

## Projection, mutations, and authorization

Public projection resolves registered model fields and intersects adapter
output with the declared attributes and relationships. Undeclared model values
are excluded even if a custom mapper returns them. Sparse fieldsets and include
linkage use the same projection helpers for HTTP and Atomic result resources.

`SeaOrmResourceMutationHandler` can be registered as both the standard base
CRUD executor and an Atomic executor. Ordinary commands use independent
transactions; an Atomic request uses one transaction for the complete batch.
Relationship mutation handlers use the same explicit storage mapping and
permission model. Omitted properties remain distinct from explicit `null`.

`AuthorizationPolicy` receives validated query plans, including their include
trees, and ordinary mutation or Atomic operation plans. `SharedAuthorization`
adapts one policy to the HTTP and Atomic interfaces; `AllOfAuthorizationPolicy`
requires every composed policy to allow the request. Resource, attribute, and
relationship permissions remain independently enforced. The lower-level
`SeaOrmReadGuard` has no HTTP headers and can add query-specific checks.

## Limits and verification

`ExecutionLimits` configures include depth and breadth, filter complexity,
relationship member count, and Atomic batch size. Runtime budgets can limit
included resources and include queries during SeaORM loading. Custom loaders
can consume those budgets before accepting related rows or issuing queries.

Tests cover typed scalar conversion and identifiers, filters and sorting,
fieldsets, standard foreign-key and join-table includes, denied operations,
invalid configuration, execution limits, and base/Atomic transaction
rollback. The SQLite integration target exercises the common relationship
paths. PostgreSQL integration tests require `SEAMARK_TEST_DATABASE_URL`.

## Current boundaries

Computed attributes are read-only and non-queryable. Relationship storage
shapes outside to-one foreign keys, to-many foreign keys, and two-column join
tables require custom mappers or executors. Application-defined business
authorization and custom persistence behavior remain application choices.
Full JSON:API and Atomic Operations specification conformance is outside the
claim of this mapping layer.
