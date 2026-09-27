# JSON:API conformance

**Status: proposal for review; implementation is partial.** The Rust crate
provides JSON:API document types and structural checks plus query and Atomic
Operations prototypes, but no complete conformance suite exists yet.

The validator distinguishes request-style local IDs from response resource
IDs, rejects objects that contain both `id` and `lid`, rejects duplicate
resource identities scoped by type across primary and included data, and
checks that every included resource is reachable from primary data through
relationship linkage. Link and member semantics and a complete context-aware
validation matrix remain unfinished.

The initial complete-release objective is full normative compliance with the JSON:API 1.1 base specification and complete support for its Atomic Operations extension. Other third-party extensions and profiles are deferred.

Tests should provide evidence for normative protocol behavior, including negotiation, valid and invalid requests, response documents, and extension behavior. Query tests should verify the documented server filter and pagination contracts; mutation tests should cover ordered Atomic Operations, local-ID references, transaction rollback, and the absence of partial success. M6 conformance work validates PostgreSQL first; M7 is responsible for the separate SQLite backend and cross-backend equivalence matrix for shared query and transaction semantics.

Atomic Operations planning checks operation shapes, explicit registry fields, relationship target types, ordered local-ID references, and application-resolved collection, resource, and relationship `href` targets. Planned resource mutations expose mapped changesets, and relationship operations expose mapped internal fields. A standalone Axum route negotiates the Atomic Operations extension and exercises success, malformed requests, unsupported media types, authorization denial, operation failure, and all three `href` target classes. Typed SeaORM handlers exercise resource CRUD and to-one foreign-key persistence in a shared transaction, with rollback coverage; a custom many-to-many executor test proves application dispatch, local-ID resolution, and rollback on a later failure. Relation-specific persistence remains application-defined, and normative request/response and error cases remain uncovered. This objective describes intended scope only. It must not be read as a claim that Seamark currently implements any JSON:API behavior.

## Initial requirement-to-test matrix

This is a gap-tracking matrix, not a line-by-line completion claim. “Partial”
means the cited tests cover selected behavior only; M6 must expand the entries
against every applicable JSON:API 1.1 and Atomic Operations normative
requirement before declaring conformance.

| Requirement area | Current evidence | Status and remaining work |
| --- | --- | --- |
| Top-level document content, `data`/`errors` exclusivity, non-empty errors, omitted versus explicit `null`, and duplicate JSON members | `tests/document.rs`: `rejects_documents_without_data_errors_or_meta`, `rejects_documents_containing_both_data_and_errors`, `rejects_an_empty_errors_array`, `distinguishes_explicit_null_from_omitted_document_and_relationship_data`, `rejects_duplicate_document_members` | Partial. Audit top-level `jsonapi`, extension/profile, links, and context-specific request/response rules. |
| Resource and identifier `type`, `id`, and `lid` shapes; response resources require persistent IDs | `tests/document.rs`: `accepts_local_id_resource_objects_but_requires_response_ids`, `rejects_resources_with_an_empty_type`, `rejects_identifiers_without_type_or_identity`, `resource_objects_and_identifiers_must_not_contain_both_id_and_lid` | Partial. Check empty identifier semantics and request/response rules against the complete normative text. |
| Resource attributes, relationships, relationship linkage, and identity uniqueness | `tests/document.rs`: `preserves_null_attribute_values_and_distinguishes_omitted_attributes`, `relationship_linkage_round_trips_null_single_and_multiple_identifiers`, `validates_relationship_identifier_objects_and_local_ids`, `rejects_duplicate_resource_identifiers_in_a_collection`; `tests/http.rs`: `collection_projects_only_declared_fields_and_preserves_nulls`, `rejects_relationship_linkage_to_an_unregistered_target_type` | Partial. Add member-name conflict and link/relationship-object validation coverage; registry target checks are framework policy in addition to protocol validation. |
| Compound documents and included-resource reachability | `tests/document.rs`: `rejects_included_resources_without_primary_data`, `validates_included_resource_reachability_through_relationship_linkage`, `rejects_duplicate_resource_identifiers_in_a_collection` | Reachability through nested relationship linkage and duplicate identities are checked. Add broader local-identity and edge-case cases; link semantics are not validated. |
| Error objects and HTTP error representation | `tests/document.rs`: `serializes_error_sources_and_status_as_strings`; `tests/http.rs`: structured errors for missing resources, unsupported queries, authorization, negotiation, and adapter failure; `tests/atomic_http.rs`: malformed/failed Atomic requests | Partial. Error-member requirements, multiple-error behavior, status consistency, source pointers, and complete endpoint status mappings need normative tests. |
| JSON:API media-type negotiation and response headers | `tests/http.rs`: `media_negotiation_is_applied_to_both_routes`, `supports_jsonapi_and_wildcard_accept_ranges_on_both_routes`, `ignores_profile_parameters_and_rejects_unsupported_extensions`, `exact_zero_quality_overrides_wildcard_but_repeated_exact_ranges_are_combined`; `src/atomic_http.rs` unit tests for quoted extension parameters and `Accept` quality | Partial. Complete applicable media-range, extension, profile, and request `Content-Type` cases remain to be mapped and tested. |
| Collection query behavior | `tests/query.rs` and `tests/seaorm.rs`: focused filter grammar, opt-in sort, pagination, sparse fieldsets, database execution, includes, and pre-query guards; `tests/http.rs` and `tests/seaorm.rs`: opt-in Axum-to-PostgreSQL execution | Framework-defined grammar, not standardized JSON:API query syntax. Expand invalid/limit/authorization cases and document the supported boundary. |
| Atomic Operations document shape, ordering, references, results, negotiation, and rollback | `tests/atomic.rs`: planner, local-ID, result-cardinality, handler-order, and transaction rollback cases; `tests/atomic_http.rs`: extension negotiation, malformed requests, auth, operation errors, and all three `href` target classes; `tests/seaorm_mutation.rs`: typed CRUD, local-ID FK linkage, to-one and custom-dispatched to-many writes, rollback | Partial. Complete the Atomic Operations normative request/response/error/relationship operation matrix, including all edge cases and route/persistence behaviors. |
| Database backend behavior | `tests/seaorm.rs` and `tests/seaorm_mutation.rs` use PostgreSQL 17 | PostgreSQL is the first validated backend. SQLite and shared cross-backend equivalence are reserved for M7 and have no implementation evidence yet. |

The matrix must eventually cite concrete test names or conformance cases for
every applicable normative requirement. Uncovered rows stay partial or
unimplemented; implementation milestones and passing project tests alone do
not establish full conformance.
