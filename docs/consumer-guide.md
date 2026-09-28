# Consumer integration guide

This guide shows how to assemble the standard SeaORM query and mutation
components. It assumes SeaORM entities named `port`, `person`, `tag`, and
`port_tag`, with a to-one `port.owner_id` foreign key and a two-column
`port_tag` join table. Generated entities remain application types; replace
the module and column names with those from your project.

## Declare the public surface

Only registered resources and mapped fields are available to clients. Query,
include, and write capabilities are enabled independently.

```rust,ignore
use std::sync::Arc;

use seamark::registry::{
    AttributeMapping, AttributePermission, RelationshipPermission,
    ResourceDefinition, ResourcePermission, ResourceRegistry,
};
use seamark::seaorm::{
    attribute_mapping, computed_attribute_mapping, join_table_relationship_mapping,
    relationship_mapping,
};
use serde_json::json;

let display_label = AttributeMapping::new("label", "computed_label");
let computed_label = computed_attribute_mapping::<port::Entity, _>(
    display_label.clone(),
    |model| Ok(json!(format!("{} m", model.depth_m))),
);

let ports = ResourceDefinition::new("ports", "port_id")
    .allow(ResourcePermission::Create)
    .allow(ResourcePermission::Update)
    .allow(ResourcePermission::Delete)
    .allow(ResourcePermission::AtomicCreate)
    .allow(ResourcePermission::AtomicUpdate)
    .allow(ResourcePermission::AtomicDelete)
    .mapped_attribute(
        attribute_mapping::<port::Entity>("name", port::Column::Title)
            .allow(AttributePermission::Filter)
            .allow(AttributePermission::Sort)
            .allow(AttributePermission::Create)
            .allow(AttributePermission::Update)
            .allow(AttributePermission::AtomicCreate)
            .allow(AttributePermission::AtomicUpdate),
    )
    .mapped_attribute(display_label)
    .mapped_relationship(
        relationship_mapping::<port::Entity>(
            "owner", port::Column::OwnerId, "people", true,
        )
        .allow(RelationshipPermission::Include)
        .allow(RelationshipPermission::LinkageRead)
        .allow(RelationshipPermission::ResourceCreate)
        .allow(RelationshipPermission::ResourceUpdate)
        .allow(RelationshipPermission::AtomicResourceCreate)
        .allow(RelationshipPermission::AtomicResourceUpdate),
    )
    .mapped_relationship(
        join_table_relationship_mapping::<port_tag::Entity>(
            "tags",
            "tags",
            "tags",
            port_tag::Column::PortId,
            port_tag::Column::TagId,
        )
        .allow(RelationshipPermission::Include),
    );

let people = ResourceDefinition::new("people", "person_id")
    .mapped_attribute(attribute_mapping::<person::Entity>(
        "name",
        person::Column::DisplayName,
    ));
let tags = ResourceDefinition::new("tags", "tag_id").mapped_attribute(
    attribute_mapping::<tag::Entity>("name", tag::Column::Name),
);

let registry = Arc::new(ResourceRegistry::new([ports, people, tags])?);
```

`AttributeMapping::new` grants representation reads only. Filtering, sorting,
and writes require their own named `AttributePermission`. Registering a
relationship does not enable includes or changes. Related-resource reads
require `RelationshipPermission::RelatedRead`; linkage reads, includes, and
each relationship write (base and Atomic) are independent permissions. Base
resource writes and Atomic Operations also have separate resource permissions.

Implement `AuthorizationPolicy` once and wrap it in `SharedAuthorization` to
use the same application policy for query plans (including includes), ordinary
mutations, and Atomic Operations. Pass clones of that wrapper to the HTTP
builder and Atomic endpoint configuration. The policy receives the same request
headers at each entry point; its write and Atomic authorization methods deny by
default. `AllOfAuthorizationPolicy` composes policies with an all-must-allow
rule.

Create one `Arc<SharedAuthorization<_>>` and pass clones to the final builder
for HTTP and Atomic Operations, as shown below.

## Register SeaORM reads

The standard row mapper and scalar codec handle mapped columns and common
scalar values. The query adapter dispatches collection and single-resource
plans without an application forwarding adapter.

```rust,ignore
use seamark::query::PaginationConfig;
use seamark::seaorm::{AllowAllSeaOrmReadGuard, SeaOrmQueryAdapter, SeaOrmQueryExecutor};

let executor = SeaOrmQueryExecutor::<port::Entity, _, _>::mapped_with_computed(
    (*registry).clone(),
    "ports",
    vec![computed_label.clone()],
)?;
let mut queries = SeaOrmQueryAdapter::new();
queries.register(
    database.clone(),
    executor,
    Arc::new(AllowAllSeaOrmReadGuard),
    None,
)?;
let person_executor = SeaOrmQueryExecutor::<person::Entity, _, _>::mapped(
    (*registry).clone(),
    "people",
)?;
queries.register(
    database.clone(),
    person_executor,
    Arc::new(AllowAllSeaOrmReadGuard),
    None,
)?;
let tag_executor = SeaOrmQueryExecutor::<tag::Entity, _, _>::mapped(
    (*registry).clone(),
    "tags",
)?;
queries.register(
    database.clone(),
    tag_executor,
    Arc::new(AllowAllSeaOrmReadGuard),
    None,
)?;
queries.register_join_table::<port_tag::Entity>("ports", "tags", database.clone())?;
```

The HTTP `RequestAuthorizer` above is the request-level read policy and sees
the validated include tree. `AllowAllSeaOrmReadGuard` is an explicit
pass-through for direct query execution inside this already-authorized router;
use a restrictive `SeaOrmReadGuard` when executing query plans outside that
boundary. The standard adapter loads declared to-one and to-many foreign-key
relationships in batches. Ordered join tables register through the same
`register_join_table` call and preserve member order through a position column.
Related-resource routes (`GET /{type}/{id}/{relationship}`) are served when the
relationship grants `RelatedRead`. Custom include loaders remain available for
application-defined association shapes.

The `label` field above is mapped from a model function and reused by the
query executor and mutation handler. Computed attributes are included in read
and base-mutation representations, but cannot be filtered, sorted, or written
because they do not map to database columns.

## Reuse persistence for base writes and Atomic Operations

One `SeaOrmResourceMutationHandler` can serve as both the standard CRUD
executor and an Atomic executor. It shares its mapped create, update, and
delete operations; each base command commits independently and an Atomic
request runs in one transaction.

```rust,ignore
use seamark::http::ApiBuilder;
use seamark::limits::ExecutionLimits;
use seamark::seaorm::SeaOrmColumnValueCodec;
use seamark::seaorm_mutation::{
    SeaOrmAtomicOperationDispatcher, SeaOrmBaseMutationAdapter,
    SeaOrmBaseMutationExecutor, SeaOrmAtomicOperationExecutor,
    SeaOrmResourceMutationHandler,
};

let port_handler = Arc::new(SeaOrmResourceMutationHandler::<
    port::Entity,
    SeaOrmColumnValueCodec<port::Entity>,
>::new_with_computed_attributes(
    &registry,
    "ports",
    SeaOrmColumnValueCodec::default(),
    vec![computed_label],
)?);
let base_executor: Arc<dyn SeaOrmBaseMutationExecutor> = port_handler.clone();
let atomic_executor: Arc<dyn SeaOrmAtomicOperationExecutor> = port_handler;
let mutations = Arc::new(SeaOrmBaseMutationAdapter::new(
    database.clone(),
    vec![base_executor],
));
let atomic = Arc::new(SeaOrmAtomicOperationDispatcher::new(vec![atomic_executor]));

let pagination = PaginationConfig::new(1, 25, Some(100), Some(10_000))?;
let authorization = Arc::new(SharedAuthorization::new(MyAuthorizationPolicy));
let app = ApiBuilder::new(registry.clone(), authorization.clone())
    .queries(Arc::new(queries), pagination)
    .mutations(mutations)
    .atomic_operations(database.clone(), authorization.clone(), atomic)
    .limits(
        ExecutionLimits::new()
            .max_include_depth(3)
            .max_include_relationships(8)
            .max_filter_nodes(32)
            .max_relationship_members(100)
            .max_atomic_operations(50)
            .max_included_resources(500)
            .max_include_queries(12),
    )
    .try_build()?;
```

Optional builder flags stay off by default: `.links()` emits document and
resource `self` links plus pagination links, and `.jsonapi_fallback()` installs
a scoped JSON:API `404`/`405` fallback on the component router.

The handlers for configured writable resources must be registered for each
enabled operation. `.try_build()` checks standard query, base mutation, and
Atomic executor coverage before serving requests. Custom adapters may provide
their own registry validation or keep dynamic dispatch.

For to-many writes, register a `SeaOrmJoinTableMutationHandler` for a two-column
join table (use `new_with_insert_columns` when the join table has additional
required columns, or an ordered join-table mapping to preserve member order), or
a `SeaOrmToManyForeignKeyMutationHandler` for a direct foreign key on the
related entity. Nullable foreign keys support add, remove, and replace;
non-nullable foreign keys support add and transfer only. The base mutation
adapter composes those relationship executors with resource create/update inside
the same transaction. Unsupported association shapes can use custom executors.

`RequestAuthorizer::authorize_mutation` denies by default.
`SharedAuthorization` lets one `AuthorizationPolicy` govern HTTP reads and
includes, ordinary mutations, and Atomic Operations; its mutation and Atomic
checks also deny by default. `AllOfAuthorizationPolicy` composes several
policies with an all-must-allow rule. The query executor guard remains a
separate lower-level hook because it runs without HTTP headers.

This standard path removes per-resource forwarding adapters, repeated
SeaORM-to-JSON scalar conversion, HTTP projection code, and duplicate base
versus Atomic CRUD handlers. Seamark performs those mechanics from the
registry and typed executor registrations. The application still supplies its
database, public field and relationship choices, authorization decisions, and
custom behavior for computed fields or nonstandard associations.

## Current boundary

An in-memory consumer fixture can keep its store, custom query and mutation
adapters, and Atomic executor. It can still use `ApiBuilder`,
`SharedAuthorization`, named registry permissions, `ExecutionLimits`, and
`project_resource` to share route composition, policy checks, limits, and
resource projection. Those helpers simplify the Seamark boundary; any change to
the fixture's storage implementation is independent.

Association shapes outside two-column, ordered, and extra-column join tables
and direct foreign keys still need custom mappers or executors. Extensions
beyond Atomic Operations and profile application are out of scope. Computed
attributes use an explicit mapper and remain read-only and non-queryable.
Custom SeaORM include loaders receive a per-request
`SeaOrmRuntimeBudget` when `max_included_resources` or `max_include_queries` is
configured. They can consume row budget as they accept related records and
query budget before each database query. Seamark checks returned row counts
after loading, including results from non-SeaORM adapters.
