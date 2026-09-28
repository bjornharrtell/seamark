# Implementation plan

This document is the current roadmap and status for issue #1. It describes the
first-useful release profile, milestone completion, verification, and explicit
follow-up boundaries. Detailed requirement-to-test evidence lives in
[`docs/design/conformance.md`](design/conformance.md); this plan intentionally
does not duplicate its per-case matrix.

## First-useful profile

- Rust library using Axum for opt-in HTTP routes and SeaORM for typed database
  execution. PostgreSQL is the primary validated backend; SQLite provides core
  parity for the shared fixtures.
- Public resource types, identifiers, attributes, and relationships are mapped
  explicitly. ORM naming and association shapes are not inferred.
- Collection and single-resource reads support registered includes and sparse
  fieldsets. Collection query support is equality/null filters, repeated-filter
  OR, opt-in sorting, and configured page-number/page-size pagination. On
  single-resource routes, only `include` and `fields[type]` are supported.
  Unsupported query parameters are rejected before authorization or execution.
- Base resource POST/PATCH/DELETE and relationship-linkage GET/PATCH/POST/DELETE
  use a separate mutation adapter. Base mutations are not routed through
  Atomic planning. Mutation routes are opt-in; the default router remains
  read-only.
- The opt-in Atomic Operations endpoint supports ordered resource and
  relationship operations, local IDs, typed join-table and nullable direct-FK
  relationship handlers, result validation, and transaction rollback.
- Authorization, resource limits, include loading, href resolution, typed value
  codecs, and unsupported association dispatch remain explicit application
  hooks.

This is a bounded implementation profile, not a claim of complete JSON:API or
Atomic conformance. Applicable MUST requirements for the exposed behavior are
release-blocking. A `Partial` row in the conformance matrix may describe
broader optional, conditional, or out-of-profile combinations and is not by
itself a release blocker. Unsupported inputs still receive or are ignored with
the response required by the specification.

## Milestones

| # | Milestone | Status | Completion boundary |
| --- | --- | --- | --- |
| 0 | Design baseline | Complete | The supported profile, architecture, resource mapping, query grammar, and explicit deferrals are documented. |
| 1 | Rust crate and protocol foundation | Complete | JSON:API document/resource/relationship/error models and structural validation are implemented; omitted values remain distinct from explicit `null`. |
| 2 | Registry and explicit mappings | Complete | Resource types and public fields resolve through validated, adapter-independent registry mappings. |
| 3 | Axum GET vertical slice | Complete | Collection and single-resource GET routes negotiate JSON:API, authorize before adapters, project registered fields, and reject unsupported queries. |
| 4 | Read planning and SeaORM execution | Complete | The documented filter/sort/page/fieldset/include plan runs through PostgreSQL-backed SeaORM; HTTP validation and limits precede query execution. |
| 5 | Base mutations and bounded Atomic Operations | Complete | Base CRUD/linkage remains separate from Atomic; supported ordered Atomic operations, local IDs, results, rollback, media negotiation, and error mapping are covered. Unsupported association shapes use application executors. |
| 6 | First-release conformance and hardening | Complete | The applicable MUST requirements for the first-useful profile have positive/negative evidence; local gates and PR CI passed for the implementation. The matrix remains explicitly partial outside the profile. |
| 7 | SQLite core parity | Complete | Shared PostgreSQL/SQLite query, identifier, relationship, mutation, result, and rollback fixtures pass. This is core parity, not an exhaustive claim of engine equivalence. |

## Release gate and verification

The first-useful release is gated on the profile above: resource reads and
mutations, relationship linkage, the documented query subset, and ordered
Atomic resource/relationship operations with local IDs and transaction
rollback. Its exposed paths must meet their applicable normative MUSTs, pass
PostgreSQL and shared SQLite checks, and document unsupported inputs and
application-dispatch boundaries. The conformance matrix records those
requirements and evidence.

Latest verified suite: **242 integration tests** (including **16 SQLite test
functions**), **13 unit tests**, and **1 doctest**. The isolated PostgreSQL-backed
all-feature suite, Clippy with warnings denied, rustdoc with warnings denied,
formatting, and diff checks pass; both PR CI jobs passed on the code-identical
head before this documentation-only cleanup.

```sh
SEAMARK_TEST_DATABASE_URL=<isolated-postgres-url> cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --all-features --no-deps
cargo fmt --check
git diff --check
```

PostgreSQL-backed tests skip with a skip message when
`SEAMARK_TEST_DATABASE_URL` is unset, so `cargo test` passes without a database.
CI sets the variable so the full suite runs; SQLite tests run under
`--all-features`.

Continue development in reviewable commits on the issue-tracking PR. Update
this roadmap only when milestone status, profile boundaries, or a design
decision changes; add detailed case evidence to
[`docs/design/conformance.md`](design/conformance.md).
Delegate genuinely independent work only after agreeing on interfaces and file
ownership, then review and integrate results before recording milestone
evidence.

## Intentional deferrals

- Applying requested profiles and reflecting them in response media types.
  Profile application is a JSON:API SHOULD and is outside this profile.
- Automatic support for every ORM association shape, ordered relationship
  storage, and join tables with additional required columns. Applications may
  provide custom executors.
- Exhaustive optional/conditional endpoint combinations and a complete
  line-by-line JSON:API/Atomic conformance suite. The conformance matrix remains
  partial; no full conformance claim is made.
- Backend parity for uncommon identifier types, engine-specific schema or
  constraint behavior, and concurrency/locking.
- Continuous refresh of IANA link-relation and BCP 47 registry snapshots.
