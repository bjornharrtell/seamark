# JSON:API conformance

**Status: proposal for review; implementation is partial.** The Rust crate
provides JSON:API document types and structural checks plus query and Atomic
Operations prototypes, but no complete conformance suite exists yet.

The initial validator distinguishes request-style local IDs from response
resource IDs and rejects duplicate resource object identities scoped by type
across primary and included data. Full compound-document linkage, including
linkage reachability and local-ID consistency across relationship identifiers,
is not implemented yet.

The initial complete-release objective is full normative compliance with the JSON:API 1.1 base specification and complete support for its Atomic Operations extension. Other third-party extensions and profiles are deferred.

Tests should provide evidence for normative protocol behavior, including negotiation, valid and invalid requests, response documents, and extension behavior. Query tests should verify the documented server filter and pagination contracts; mutation tests should cover ordered Atomic Operations, local-ID references, transaction rollback, and the absence of partial success. M6 conformance work validates PostgreSQL first; M7 is responsible for the separate SQLite backend and cross-backend equivalence matrix for shared query and transaction semantics.

Atomic Operations planning checks operation shapes, explicit registry fields, relationship target types, ordered local-ID references, and application-resolved relationship `href` targets. Planned resource mutations expose mapped changesets, and relationship operations expose mapped internal fields. A standalone Axum route negotiates the Atomic Operations extension and exercises success, malformed requests, unsupported media types, authorization denial, operation failure, and relationship `href` routing. Typed SeaORM handlers exercise resource CRUD and to-one foreign-key persistence in a shared transaction, with rollback coverage. To-many writes still require application executors, and several normative request/response and error cases remain unimplemented. This objective describes intended scope only. It must not be read as a claim that Seamark currently implements any JSON:API behavior.
