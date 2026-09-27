# Implementation plan

This is the evolving implementation roadmap for issue #1. The release target
is the full normative JSON:API 1.1 base specification plus the complete Atomic
Operations extension. The intended first stack is Axum, then SeaORM with
PostgreSQL. Public resource names and fields must be mapped explicitly; query
support is deliberately focused on equality/null filters, opt-in sorting, and
page-number/page-size pagination.

Milestone status records the current implementation, not the release target.
Completing a milestone demonstrates only its listed scope; it does not imply
full JSON:API conformance or release readiness.

## Milestones

| # | Milestone and scope | Exit criteria and testing evidence | Status |
| --- | --- | --- | --- |
| 0 | **Design baseline.** Keep the design documents aligned on the initial scope, resource mapping model, query grammar, and implementation boundaries. | Normative JSON:API 1.1 and Atomic Operations are explicit release targets; Axum and SeaORM/PostgreSQL are the initial adapters; public mappings and supported query behavior are documented. | Complete |
| 1 | **Rust crate and protocol foundation.** Build the library foundation and JSON:API document, resource, relationship, and error representations. Add only structural validation at this stage. | Verified: 19 integration tests pass. Coverage includes serialization, explicit-null versus omission, nullable resource `id`/`lid` request objects with response-only persistent-ID validation, null/one/many relationship linkage with `id` and `lid`, empty collections, missing identities, and duplicate resource identities scoped by type across primary and included data. `cargo fmt --all`, `cargo test --all-targets --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo doc --no-deps --all-features`, and `git diff --check` pass. Duplicate JSON-member rejection is a separate parser test, not the resource-identity check. Compound linkage reachability and local-ID consistency across relationship linkages remain deferred. Keep validation gaps visible; do not imply complete normative validation. | Complete |
| 2 | **Minimal resource registry and explicit mappings.** Define a small registry of public resource types and explicit identifier, attribute, and relationship mappings independent of ORM entity naming. Keep this first mapping layer minimal and adapter-independent; defer SeaORM/PostgreSQL mapping to milestone 4. | Verified: 8 registry integration tests cover valid and invalid declarations, same-category duplicate public names, reserved `id`/`type` names, identifier/attribute/relationship backing-field collisions, relationship target resolution including self-reference, resource-scoped lookup, explicit filter/sort opt-in, and all-or-nothing batch validation. Only registered resources and declared fields are exposed. No ORM persistence or broader route/query behavior is part of this milestone. | Complete |
| 3 | **Single Axum GET vertical slice.** Add exactly two GET route shapes (collection and single resource) backed by the registry and a narrow persistence-independent adapter. Implement JSON:API media-type negotiation and structured protocol errors; expose only supported capabilities and reject every unsupported query parameter. | Verified: all 13 HTTP tests and all full-project quality gates pass (40 integration tests total). Tests cover unknown types, authorization denial before any adapter call (zero adapter calls), empty collections and missing single-resource results, null-versus-omitted values, output restricted to declared attributes/relationships, accepted/rejected media types on both routes, structured errors for type/auth/negotiation/query/adapter failures, query-source reporting with leading separators, and unsupported query rejection. Negotiation ignores profile parameters, rejects unsupported extension ranges, applies specificity so exact `q=0` overrides wildcards, and combines repeated exact ranges by highest quality. Every tested success/error response has JSON:API Content-Type and `Vary: Accept`. Adapter IDs are normalized persistent IDs; invalid relationship target types become generic adapter errors. Keep filters, sorting, pagination, includes, sparse fieldsets, ORM/persistence, mutations, and Atomic Operations explicitly deferred. | Complete |
| 4 | **Read planning and SeaORM/PostgreSQL mapping and execution.** Establish the production mapping from explicit public-resource declarations to SeaORM entities and PostgreSQL, then parse and validate the focused equality and null filter grammar, repeated-filter OR behavior, opt-in sorting, page-number/page-size pagination, sparse fieldsets, and included relationships. Apply authorization hooks and resource limits before execution. | **Verified prototype evidence:** 21 query tests cover parser and plan behavior; 2 PostgreSQL integration tests exercise persisted string/OR, null, and typed numeric predicates, sorting, pagination, identifiers, sparse fieldsets, application-provided include loading, and authorization/limit rejection before execution. The typed executor resolves mapped fields to SeaORM columns, uses an explicit filter-value encoder and model mapper, and has no in-memory query fallback. **Remaining before milestone completion:** wire planned requests through Axum/adapter boundaries; finalize reusable entity, identifier, and relationship mapping APIs; verify broader database types and relation cases; and complete the integration matrix for unsupported requests and authorization/resource limits. | In progress |
| 5 | **Mutation planning and Atomic Operations.** Implement resource create/update/delete behavior and the complete Atomic Operations extension, including ordered operations, result reporting, local-ID references, and all-or-nothing execution through SeaORM transactions. | **Verified prototype evidence:** 8 Atomic Operations tests validate request/response shapes, add/update/remove and relationship-operation planning, explicit attribute/relationship mappings, ordered local IDs, URI-reference targets, result cardinality, pre-transaction authorization/limits, successful ordered PostgreSQL execution, and rollback after a later operation fails. The SeaORM runner requires an application handler, guard, and local-ID outcomes and commits only after all planned operations succeed. **Remaining before milestone completion:** connect operations to concrete CRUD/relationship mapping and Axum POST routing; negotiate the Atomic Operations extension in `Content-Type`/`Accept`; finish route-aware `href` relationship operations; and complete normative results/error/media-type tests. | In progress |
| 6 | **Normative conformance and release hardening.** Close remaining JSON:API 1.1 base-specification and Atomic Operations gaps, document the exact support boundary, and harden the first database adapter. | Maintain a requirement-to-test matrix for the full normative base specification and extension. Run applicable conformance cases plus unit, Axum, and PostgreSQL integration suites in CI; document and justify any unsupported behavior before release. The release target is complete only when all applicable normative requirements are covered and passing. | Planned |

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
  tests for Axum behavior, and PostgreSQL-backed integration tests for
  persistence and transaction guarantees. Add regression tests for fixes and
  keep CI checks aligned with the Rust toolchain and crate configuration.
- Delegate only work that is genuinely independent and can proceed concurrently
  (for example, isolated conformance-test research or a separate test fixture).
  Agree on interfaces and file ownership first, avoid concurrent edits to the
  same design or implementation surface, and integrate/review delegated
  results before recording milestone evidence.
- Track known omissions explicitly. Until milestone 6 meets its exit criteria,
  describe implementation and supported behavior as partial.

## Current implementation checkpoint

Milestones 1, 2, and 3 are complete; milestones 4 and 5 are in progress. The crate
provides JSON:API document, resource, relationship, and error structures with
limited structural validation, an explicit public resource registry, and a
read-only Axum collection/single-resource GET slice. The protocol model
preserves omitted fields separately from explicit `null`, supports request
resource `lid` values, requires response resource `id` values, and rejects
duplicate resource identities by type across primary/included data. The
registry validates public/internal field mappings and target types and
requires explicit filter/sort opt-in. The HTTP slice negotiates JSON:API
responses, authorizes before adapter calls, rejects all query strings, checks
relationship target types, and projects only declared fields.

The M4 prototype parses equality/null filter expressions, combines repeated
filters with OR, and plans opt-in sorting, explicitly configured pagination,
sparse fieldsets, and nested include paths. A separate typed SeaORM executor
executes predicates, sorting, and offset/limit in PostgreSQL; callers supply
typed filter conversion, model mapping, an authorization/limit guard, and an
include loader. **Verified:** all 63 integration tests (19 protocol, 8
registry, 13 HTTP, 21 query, 7 Atomic Operations unit, and 3 PostgreSQL) pass, including focused
PostgreSQL reads for database-side OR/string/null/numeric filters, sorting,
pagination, fieldset projection, include loading, and pre-query guard
rejections. M5 adds an Atomic Operations document/planner with operation and
local-ID validation plus a transaction runner that invokes application
handlers in order, checks result identities, and rolls back on failure. **The
current M5 evidence is 8 Atomic Operations tests**, including a PostgreSQL
test for local-ID linkage across two adds and rollback after a later update
fails. Formatting, warning-free Clippy, all 71 integration tests with
PostgreSQL 17, rustdoc, and whitespace checks pass. The current
Axum handlers do not yet use the read executor or expose mutation/Atomic
Operations routes. Concrete mutation mapping, extension media negotiation,
and full normative conformance remain incomplete; no release conformance
claim is made.
