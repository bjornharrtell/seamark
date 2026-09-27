# Implementation plan

This is the evolving implementation roadmap for issue #1. The release target
is the full normative JSON:API 1.1 base specification plus the complete Atomic
Operations extension. The intended stack is Axum and SeaORM, with PostgreSQL as the first validated
database backend and SQLite as a separately validated second backend in M7.
Public resource names and fields must be mapped explicitly; query support is
deliberately focused on equality/null filters, opt-in sorting, and
page-number/page-size pagination.

Milestone status records the current implementation, not the release target.
Completing a milestone demonstrates only its listed scope; it does not imply
full JSON:API conformance or release readiness.

## Milestones

| # | Milestone and scope | Exit criteria and testing evidence | Status |
| --- | --- | --- | --- |
| 0 | **Design baseline.** Keep the design documents aligned on the initial scope, resource mapping model, query grammar, and implementation boundaries. | Normative JSON:API 1.1 and Atomic Operations are explicit release targets; Axum and SeaORM are the initial adapters; PostgreSQL is the first validated database and SQLite is reserved as the M7 second backend; public mappings and supported query behavior are documented. | Complete |
| 1 | **Rust crate and protocol foundation.** Build the library foundation and JSON:API document, resource, relationship, and error representations. Add only structural validation at this stage. | Verified: 19 integration tests pass. Coverage includes serialization, explicit-null versus omission, nullable resource `id`/`lid` request objects with response-only persistent-ID validation, null/one/many relationship linkage with `id` and `lid`, empty collections, missing identities, and duplicate resource identities scoped by type across primary and included data. `cargo fmt --all`, `cargo test --all-targets --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo doc --no-deps --all-features`, and `git diff --check` pass. Duplicate JSON-member rejection is a separate parser test, not the resource-identity check. Compound linkage reachability and local-ID consistency across relationship linkages remain deferred. Keep validation gaps visible; do not imply complete normative validation. | Complete |
| 2 | **Minimal resource registry and explicit mappings.** Define a small registry of public resource types and explicit identifier, attribute, and relationship mappings independent of ORM entity naming. Keep this first mapping layer minimal and adapter-independent; defer SeaORM/database mapping to milestone 4. | Verified: 8 registry integration tests cover valid and invalid declarations, same-category duplicate public names, reserved `id`/`type` names, identifier/attribute/relationship backing-field collisions, relationship target resolution including self-reference, resource-scoped lookup, explicit filter/sort opt-in, and all-or-nothing batch validation. Only registered resources and declared fields are exposed. No ORM persistence or broader route/query behavior is part of this milestone. | Complete |
| 3 | **Single Axum GET vertical slice.** Add exactly two GET route shapes (collection and single resource) backed by the registry and a narrow persistence-independent adapter. Implement JSON:API media-type negotiation and structured protocol errors; expose only supported capabilities and reject every unsupported query parameter. | Verified: all 13 HTTP tests and all full-project quality gates pass (40 integration tests total). Tests cover unknown types, authorization denial before any adapter call (zero adapter calls), empty collections and missing single-resource results, null-versus-omitted values, output restricted to declared attributes/relationships, accepted/rejected media types on both routes, structured errors for type/auth/negotiation/query/adapter failures, query-source reporting with leading separators, and unsupported query rejection. Negotiation ignores profile parameters, rejects unsupported extension ranges, applies specificity so exact `q=0` overrides wildcards, and combines repeated exact ranges by highest quality. Every tested success/error response has JSON:API Content-Type and `Vary: Accept`. Adapter IDs are normalized persistent IDs; invalid relationship target types become generic adapter errors. The original default router continues to reject queries; opt-in collection query support is M4. Keep mutations and Atomic Operations explicitly deferred. | Complete |
| 4 | **Read planning and SeaORM/PostgreSQL mapping and execution.** Establish the production mapping from explicit public-resource declarations to SeaORM entities and PostgreSQL, then parse and validate the focused equality and null filter grammar, repeated-filter OR behavior, opt-in sorting, page-number/page-size pagination, sparse fieldsets, and included relationships. Apply authorization hooks and resource limits before execution. | **Verified prototype evidence:** 21 query tests cover parser and plan behavior; 2 PostgreSQL query integration tests exercise persisted string/OR, null, typed numeric, and boolean predicates, sorting, pagination, identifiers, sparse fieldsets, application-provided include loading, authorization/limit rejection, and invalid typed filter rejection before execution. A shared PostgreSQL/SQLite case now covers string primary keys, string filter literals, included to-one linkage, and normalized string IDs. The fallible `SeaOrmQueryExecutor::new` validates the registered identifier and filterable/sortable attribute columns against its typed entity before execution; a regression test covers invalid identifier and query-field mappings. `SeaOrmFilterValueCodec` provides the typed query-literal conversion contract; typed mutations use the complementary `SeaOrmMutationValueCodec`, with `SeaOrmValueCodec` combining both. `router_with_query` provides opt-in collection query parsing/planning; 3 HTTP tests cover mapping, percent-decoding/repeat/unknown-parameter errors, sparse-fieldset output, includes, and authorization-before-execution. An additional PostgreSQL HTTP integration executes the SeaORM query executor through that adapter boundary and checks filtered/sorted/paginated data, relationships, includes, and root/included fieldsets. **Remaining before milestone completion:** broaden database identifier/type and relation cardinality cases, refine relationship mapping hooks, and complete the unsupported-request and authorization/resource-limit integration matrix. PostgreSQL is the first validated backend; SQLite is outside M4 and reserved for M7. | In progress |
| 5 | **Mutation planning and Atomic Operations.** Implement resource create/update/delete behavior and the complete Atomic Operations extension, including ordered operations, result reporting, local-ID references, and all-or-nothing execution through SeaORM transactions. | **Verified prototype evidence:** 14 Atomic Operations tests validate request/response shapes, add/update/remove and relationship-operation planning, mapped changesets (including omitted versus explicit `null`), local-ID ordering and self-reference rejection, URI-reference targets, collection/resource/relationship `href` resolution, result cardinality and operation-specific response data shape/identity, pre-transaction authorization/limits, and rollback. They also verify shared base JSON:API checks for Atomic top-level members and embedded resource/relationship links and structure. Atomic HTTP parsing rejects duplicate JSON member names recursively before typed decoding. Two PostgreSQL-backed HTTP tests cover extension negotiation, successful results, malformed documents, query rejection, unsupported media types, authorization and limit denial, operation failure, database failure, JSON:API response headers/status consistency, operation source pointers, and resolved collection/resource/relationship href targets reaching the handler. Two PostgreSQL mutation tests exercise typed CRUD, local-ID FK linkage, to-one clearing/replacement, collection-href add and resource-href update/delete, and a custom-dispatched to-many join-table add/remove using the same transaction; rollback coverage includes undoing earlier custom to-many writes. A shared PostgreSQL/SQLite case adds string-primary-key query and Atomic create/update/delete plus string to-one relationship reassignment. Typed handlers now consume the shared mutation codec contract for encoding values and decoding identifiers. Five unit tests cover quoted extension parameters, `Accept` quality values, URI validation for `profile` parameters, and strict JSON request parsing. **Remaining before milestone completion:** close remaining normative request/result/error/media-type cases; relation-specific to-many persistence remains an application executor responsibility rather than inferred ORM behavior. | In progress |
| 6 | **Normative conformance and release hardening.** Close remaining JSON:API 1.1 base-specification and Atomic Operations gaps, document the exact support boundary, and harden the first database adapter. | Maintain a line-by-line requirement-to-test matrix for the full normative base specification and extension, citing positive/negative tests or applicable conformance cases. Run applicable cases plus unit, Axum, and PostgreSQL integration suites in CI; document and justify any unsupported behavior before release. The initial matrix in `docs/design/conformance.md` is a gap tracker, not complete evidence. The release target is complete only when all applicable normative requirements are covered and passing. | In progress |
| 7 | **SQLite backend and cross-backend verification.** Add SQLite as a second SeaORM backend after PostgreSQL M4/M5 integration and M6 conformance work; do not couple SQLite implementation to those milestones. | **Partial evidence now integrated:** `Cargo.toml` exposes an opt-in `sqlite` feature and `.github/workflows/ci.yml` runs `cargo test --all-targets --all-features`, which executes its SQLite tests. Six `tests/seaorm_sqlite.rs` cases use isolated `sqlite::memory:` connections: the entity schema covers string, integer, nullable integer, boolean, and an enforced owner foreign key; query checks cover string OR, typed numeric/boolean and null filters, sorting, pagination, fieldsets, nullable to-one linkage, and an application include loader; typed Atomic operations cover local-ID creates, resource updates/deletes, to-one reassignment, and rollback after a later mutation fails. A generic transaction-failure test also checks rollback. PostgreSQL and SQLite share port/person fixture rows, filters, composed query cases, nullable relationship state, and the exact serialized JSON:API document in `tests/support/query_cases.rs`; both assert null linkage and no included resource for an absent to-one target. The shared eleven-operation Atomic request in `tests/support/atomic_cases.rs` asserts identical complete result documents and final database state on both backends, covering collection/resource/relationship `href` targets, custom-dispatched to-many add/remove, and rollback after an earlier write succeeds and a later operation fails. A second shared case now verifies string primary-key query and Atomic CRUD plus to-one relationship reassignment on both engines. **Remaining acceptance criteria:** expand identifier/type and relationship-cardinality cases, document intentional capability differences, and broaden the common Atomic matrix. SQLite remains partial, not a second fully validated backend. | In progress |

## Delivery and verification workflow

- Work iteratively in a pull request tracking issue #1. Keep this roadmap
  current as implementation decisions or evidence change.
- At each milestone, update the relevant design documents and this plan:
  record status, decisions, verified exit evidence, and remaining gaps. Mark a
  milestone complete only after its criteria and tests pass.
- Commit coherent milestone work in the issue-tracking pull request. Keep
  commits reviewable and do not treat a commit or merged milestone as proof of
  full release conformance.
- Prefer focused unit tests for protocol and planning logic, adapter-level
  tests for Axum behavior, and backend-specific integration tests for
  persistence and transaction guarantees. M4-M6 validate PostgreSQL first;
  M7 adds SQLite using the same shared behavior matrix where capabilities
  overlap. Add regression tests for fixes and keep CI checks aligned with the
  Rust toolchain and crate configuration.
- Delegate only work that is genuinely independent and can proceed concurrently
  (for example, isolated conformance-test research or a separate test fixture).
  Agree on interfaces and file ownership first, avoid concurrent edits to the
  same design or implementation surface, and integrate/review delegated
  results before recording milestone evidence.
- Track known omissions explicitly. Until milestone 6 meets its exit criteria,
  describe implementation and supported behavior as partial.

## Current implementation checkpoint

Milestones 1, 2, and 3 are complete; milestones 4, 5, and 6 are in progress. The crate
provides JSON:API document, resource, relationship, and error structures with
limited structural validation, an explicit public resource registry, and a
read-only Axum collection/single-resource GET slice. The protocol model preserves omitted fields separately from explicit `null`,
supports request resource `lid` values, requires response resource `id`
values, and rejects duplicate resource identities by type across
primary/included data. The registry validates public/internal field mappings
and target types and requires explicit filter/sort opt-in. Both GET routers
negotiate JSON:API responses, authorize before adapter calls, check
relationship target types, and project only declared fields. The default
router rejects all query strings; the opt-in query router handles planned
collection query parameters and continues to reject query parameters on
single-resource GETs.

The M4 prototype parses equality/null filter expressions, combines repeated
filters with OR, and plans opt-in sorting, explicitly configured pagination,
sparse fieldsets, and nested include paths. A separate typed SeaORM executor
executes predicates, sorting, and offset/limit in PostgreSQL; callers supply
typed filter conversion (including typed numeric and boolean values), model
mapping, an authorization/limit guard, and an include loader. The opt-in Axum
collection-query adapter validates and authorizes before adapter execution.
HTTP unit coverage verifies percent
decoding, duplicate/unknown parameter handling, sorting/filter/page mapping,
fieldset projection, includes, and authorization order. A PostgreSQL-backed
HTTP integration drives the SeaORM executor and verifies filtered, sorted,
paginated resources, relationship linkage, included resources, and sparse
fieldsets. Shared PostgreSQL/SQLite query tests cover nullable to-one linkage
and an application-defined self-referential to-many mapping, including two
related resources loaded into `included` through the adapter contract. A
PostgreSQL route regression verifies invalid queries are rejected
before adapter execution, maps SeaORM authorization and resource-limit failures
to HTTP 403 and 413, and proves both guarded outcomes occur before SQL against
an intentionally absent table. The registry rejects resource type and public field names that do
not meet JSON:API member-name rules. The fallible
`SeaOrmQueryExecutor::new` validates the registered identifier and all
filterable/sortable attribute columns against its entity at construction;
invalid mappings have a focused regression test. The shared
`SeaOrmFilterValueCodec` and `SeaOrmMutationValueCodec` traits now provide
typed query/mutation conversion hooks, with `SeaOrmValueCodec` for shared
implementations. The complete all-features suite passes with 115 integration
tests and 5 unit tests, including 6 isolated SQLite tests and PostgreSQL
integration cases. Formatting, warning-free Clippy, rustdoc, and whitespace
checks pass. M4 remains in progress pending broader database type/relation
cases, expanded authorization/resource-limit, and unsupported-request
coverage.

M5 now adds an Atomic Operations document/planner with operation and local-ID
validation, public-to-internal resource changesets, and relationship field
resolution. The standalone Axum `POST /operations` router enforces quoted
Atomic Operations extension negotiation, rejects query strings, returns
result/error documents, and passes request headers to authorization.
`SeaOrmResourceMutationHandler` provides typed resource CRUD and to-one
foreign-key updates using the application-supplied mutation codec; a
dispatcher composes these with custom executors. Applications can resolve
relationship `href` routes before planning. The transaction runner invokes
handlers in order, checks result identities, and rolls back on failure.
**Current focused evidence:** 14 Atomic Operations tests, 2 PostgreSQL-backed
HTTP tests, 2 PostgreSQL mutation tests, and 5 unit tests covering negotiation
and strict JSON request parsing. A shared PostgreSQL/SQLite case verifies
string primary-key query/filter behavior, included to-one linkage, Atomic
create/update/delete, and string-key relationship reassignment. Atomic
result validation now requires resource add/update `data` to be a valid
response resource with an ID and matching operation type/known ID, and
forbids result `data` for relationship and remove operations. A custom
to-many join-table handler is verified through the dispatcher, shared
transaction, local-ID resolution, and rollback; each application's relation
mapping/persistence handler remains explicit. Atomic documents and embedded
resource data now reuse base JSON:API validation for top-level links and
`jsonapi` members, resource links, and relationship object structure/links.
Additional normative request/result/error/media-type cases remain incomplete.
Resource, collection, and relationship href resolution now has planner, HTTP,
and PostgreSQL mutation coverage. Full
normative conformance remains the M6 objective. M7 adds a partial opt-in SQLite slice. PostgreSQL and SQLite now assert one
shared complete query response document and one shared eleven-operation
Atomic result document with matching persisted state. The shared Atomic case
covers collection, resource, and relationship `href` targets, custom
to-many add/remove dispatch, proves removing one of two relationship members
leaves the other persisted, and verifies rollback of a later relationship add
when a subsequent operation fails. Broader
string and integer identifier/type and relationship coverage now runs through
both query and Atomic paths. Broader identifier/type/relationship-cardinality
cases, a wider common Atomic matrix, and capability-difference documentation
remain open. No release conformance claim is made.

M6 conformance work has begun with a gap-tracking requirement-to-test matrix.
Document validation now rejects simultaneous `id`/`lid`, requires persistent
IDs for response resource objects and relationship identifiers, requires
relationship `lid` references to resolve to a resource object with the same
type and local ID, checks
conflicting or invalid resource type/field names, rejects unreachable included
resources, empty relationship objects, and error objects without any defined
member. It validates link `href` URI references, registered-token or
absolute-URI relation types, BCP 47 `hreflang` syntax, JSON Pointer syntax for
error sources, HTTP status strings in the 100-599 range, and absolute URIs in
`jsonapi.ext` and `jsonapi.profile`. Generated base and Atomic HTTP error tests
assert that each error object's `status` matches the HTTP response status;
Atomic HTTP tests also verify that every emitted source pointer resolves in the
original request document. Base GET and Atomic HTTP tests also verify the
permitted stop-at-first-problem strategy when a request has multiple faults,
so multi-error HTTP status selection is not used by these routes. The matrix
remains partial: remaining top-level JSON:API rules, normative Atomic
Operations edge cases, complete endpoint status mappings, and context-sensitive
request/response rules still require coverage before M6 can be complete.
Atomic operation-execution failure coverage now explicitly verifies the
permitted 422 status, JSON:API response headers, and a resolvable
`/atomic:operations/0` source pointer for the failed operation. Other error
categories remain partial.
The Atomic HTTP regression also verifies that guard limit failures return 413
with matching error status and JSON:API headers before an intentionally failing
operation handler can run; endpoint error mappings remain partial overall.
An Atomic database-acquisition failure is also verified to return a 500 error
document with matching JSON:API headers and status.
Document validation now explicitly covers empty `id` and `lid` strings as
opaque JSON:API string identifiers, including local-ID relationship resolution.
Unrecognized unique members are also verified to be ignored across base
document, JSON:API, resource, relationship, and identifier objects.
The document decoder rejects non-object roots and array-shaped resource,
relationship, identifier, error, error-source, and JSON:API objects.
Atomic Operations documents now ignore unrecognized members per JSON:API
processing rules while explicitly rejecting the forbidden base `data` and
`included` members. Atomic HTTP body parsing now rejects duplicate JSON object
member names recursively before typed decoding, with nested-duplicate and
trailing-value regression tests. The Atomic document, operation, reference,
and result types also reject Serde's sequence-form representations where
JSON:API requires objects. Base GET media negotiation now validates quoted
absolute profile URI lists, rejects malformed or duplicate profile
parameters, ignores Accept extensions after `q`, rejects unsupported media
parameters/extensions, and preserves the rule that unknown profiles do not
alter the base response.
The conformance matrix cites these cases but remains partial.
