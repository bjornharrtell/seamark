# Architecture and request lifecycle

**Status: partial implementation.** Seamark has an adapter-independent
resource registry and request planner, a composable `ApiBuilder`, standard
SeaORM query and mutation adapters, shared resource projection, runtime limits,
and transaction runners for ordinary and Atomic writes. Standard relationship
loading covers declared foreign-key and two-column join-table mappings.
Nonstandard associations and complete JSON:API/Atomic Operations conformance
remain outside the current support claim.

## Boundaries

The request path separates protocol handling, public metadata, application
policy, and persistence:

1. Axum routes parse JSON:API requests and serialize protocol responses.
2. The protocol layer validates resource documents, query parameters, and
   negotiated capabilities.
3. The registry explicitly declares public resources, fields, relationships,
   and operation permissions. Database columns and ORM relationships are not
   exposed automatically.
4. Planners resolve requests into validated read or mutation plans.
5. Application policy provides authorization, custom validation, business
   rules, and link customization.
6. SeaORM adapters execute planned database work using registered typed
   entities and explicit codecs.

## Routes and reads

`ApiBuilder` enables simple reads, planned queries, ordinary mutations, and
Atomic Operations independently. Collection reads and single-resource reads
use validated plans when the query adapter is enabled. Filters, sorting, and
pagination apply to collections; includes and sparse fieldsets also apply to
single resources. Invalid plans are rejected before authorization or database
execution where possible.

The standard SeaORM query adapter maps explicitly registered entity columns,
uses common scalar codecs, and dispatches collection and single-resource
queries without a forwarding adapter. Registered to-one foreign keys,
to-many foreign keys, and join tables use batched include loading. Other
association shapes can provide custom loaders. All results pass through the
same registry-based projection, which excludes undeclared fields.

## Mutations and transactions

Base requests are validated and mapped to `MutationCommand` values before
authorization and adapter execution. Changesets preserve the difference
between an omitted property and an explicit `null`. A base command executes in
its own transaction. Standard SeaORM CRUD handlers can serve as both base and
Atomic executors; relationship handlers use explicit storage and permission
metadata and can compose with resource writes in the same transaction.

Atomic Operations have an independent endpoint opt-in, plan, permission set,
guard, and handler. The complete operation batch runs in one transaction and
rolls back when an operation fails. Registry permissions for ordinary writes
do not implicitly enable Atomic writes, or vice versa.

## Authorization, limits, and errors

`RequestAuthorizer` receives complete validated read plans, including include
trees, and separately authorizes ordinary mutations. `AtomicOperationsGuard`
authorizes the planned Atomic batch. `SharedAuthorization` adapts one
`AuthorizationPolicy` to both interfaces; `AllOfAuthorizationPolicy` composes
policies using an all-must-allow rule. A lower-level `SeaOrmReadGuard` remains
available for query-specific checks that do not use HTTP headers.

`ExecutionLimits` composes common include, filter, relationship-linkage, and
Atomic batch limits with application checks. Runtime budgets cap related rows
and include queries while standard or custom SeaORM loaders expand a request.
Adapter errors are translated at the HTTP boundary to JSON:API error
categories.

Capability negotiation exposes only configured behavior; unsupported behavior
must fail explicitly rather than be silently approximated. Complete normative
JSON:API and Atomic Operations behavior remains future work.
