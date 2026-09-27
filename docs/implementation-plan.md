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
| 2 | **Minimal resource registry and explicit mappings.** Define a small registry of public resource types and explicit identifier, attribute, and relationship mappings independent of ORM entity naming. Keep this first mapping layer minimal and adapter-independent; defer SeaORM/PostgreSQL mapping to milestone 4. | Focused tests cover valid and invalid registry/mapping declarations and lookup by public resource name. Document that only registered resources and declared fields are exposed. No ORM persistence or broader route/query behavior is part of this milestone. | Planned |
| 3 | **Single Axum GET vertical slice.** Add only collection and single-resource GET routes backed by the registry and a narrow adapter boundary. Implement JSON:API media-type negotiation and structured protocol errors; expose only supported capabilities and reject unsupported query parameters. | Axum/adapter tests cover unknown resources; authorization denial before any adapter call; empty collections and missing single-resource results; null-versus-omitted serialization; accepted/rejected media types and structured errors; and rejection of unsupported query parameters. Keep filtering, sorting, pagination, includes, and persistence out of this slice unless separately implemented and tested. | Planned |
| 4 | **Read planning and SeaORM/PostgreSQL mapping and execution.** Establish the production mapping from explicit public-resource declarations to SeaORM entities and PostgreSQL, then parse and validate the focused equality and null filter grammar, repeated-filter OR behavior, opt-in sorting, page-number/page-size pagination, sparse fieldsets, and included relationships. Apply authorization hooks and resource limits before execution. | Mapping tests exercise identifiers, attributes, and relationships against the SeaORM path; PostgreSQL integration tests verify persisted reads. Parser/unit tests cover valid, invalid, empty, null, repeated, and boundary inputs. Query-planning tests verify authorization, limits, sorting opt-in, and pagination semantics. Unsupported query forms fail predictably rather than being silently accepted. | Planned |
| 5 | **Mutation planning and Atomic Operations.** Implement resource create/update/delete behavior and the complete Atomic Operations extension, including ordered operations, result reporting, local-ID references, and all-or-nothing execution through SeaORM transactions. | Unit tests cover operation parsing, validation, ordering, and local-ID resolution. PostgreSQL integration tests prove successful multi-operation execution and rollback when any operation fails. Verify the extension's normative request/response and error behavior, including media-type requirements. | Planned |
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

Milestone 1 is complete: the Rust crate has JSON:API document, resource,
relationship, and error structures plus limited structural validation. The
model preserves omitted fields separately from explicit `null`, represents
resource linkage (including `lid`), and serializes error status codes as
strings. All 19 integration tests and the formatting, Clippy, documentation,
and whitespace checks recorded above pass. Generic validation accepts a
request resource object with either `id` or `lid`; response validation
specifically requires a persistent `id`. This is a protocol-model foundation
only: there are not yet HTTP routes, persistence, or full normative
conformance. Those broader goals remain partial or planned; the minimal
resource registry and mapping in milestone 2, followed by the limited Axum GET
slice in milestone 3, are next. PostgreSQL mapping remains scoped to the
SeaORM work in milestone 4. Use the registry design choices in
[`resources-and-mapping.md`](design/resources-and-mapping.md) as the next
design dependency.
