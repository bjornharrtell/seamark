# Seamark

Seamark is a Rust framework for building JSON:API servers with Axum and
SeaORM. It provides explicit resource and field mappings, planned collection
and single-resource queries, opt-in ordinary mutations, Atomic Operations,
and shared projection and validation helpers.

## Integration

Use `http::ApiBuilder` to opt into the capabilities the application serves:

```rust,ignore
let authorization = Arc::new(SharedAuthorization::new(request_policy));
let app = ApiBuilder::new(registry, authorization.clone())
    .queries(query_adapter, pagination)
    .mutations(base_mutation_adapter)
    .atomic_operations(database, authorization.clone(), atomic_handler)
    .limits(execution_limits)
    .try_build()?;
```

Capabilities are disabled until configured. Write permissions are separate
from reads, and ordinary writes are separate from Atomic Operations. The
standard SeaORM query adapter loads registered to-one and to-many foreign-key
relationships, and registered join tables, without a forwarding adapter or
application traversal loader. CRUD handlers can serve both ordinary and
Atomic mutations. Computed read-only attributes can use a registered mapping
function; custom mappers and executors remain available for more specialized
behavior and other association shapes.

See the [consumer integration guide](docs/consumer-guide.md) for resource
permissions, typed mappings, SeaORM registration, transaction behavior, and
current integration boundaries.

## Query and mutation behavior

Public resource types, identifiers, attributes, and relationships are
registered explicitly. Filtering, sorting, include traversal, and each write
operation require separate permissions. Query planning validates filters,
sorts, sparse fieldsets, and include trees before adapter execution. The
standard scalar codec handles common numeric, string, boolean, date/time,
decimal, JSON, and UUID values; applications can replace or extend codecs.

Base resource mutations each run in one transaction. To-many replacement,
addition, and removal can use explicitly configured join-table or nullable
foreign-key handlers. A base create/update that includes to-many linkage is
composed through those handlers in the same transaction. Atomic Operations
execute the whole request in one transaction. `SharedAuthorization` lets one
application policy serve HTTP reads, includes, ordinary mutations, and Atomic
Operations; `AllOfAuthorizationPolicy` composes policies with an
all-must-allow rule.

Reusable limits cover include depth and breadth, filter complexity,
relationship linkage size, Atomic batch size, and optional related-resource
and include-query budgets. SeaORM include loaders can consume runtime budgets
during expansion; other query adapters are checked against the returned include
count. Pagination limits remain an explicit configuration. Application
authorization is required for reads and writes;
`RequestAuthorizer::authorize_mutation` denies unless the application allows
the operation.

## Scope

This is not a claim of complete JSON:API 1.1 or Atomic Operations conformance.
The [design documents](docs/design/) record supported behavior and known
limits. Computed attributes are read-only and cannot be filtered or sorted.
Association shapes outside the standard foreign-key and join-table mappings
still use custom mappers or executors.
