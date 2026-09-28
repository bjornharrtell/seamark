# Conformance implementation plan

**Status: planned.** This plan defines the workstreams required to move from the
bounded first-release profile to a fuller JSON:API 1.1 and Atomic Operations
conformance claim. It replaces the archived first-release plan
([`docs/archive/implementation-plan-first-release.md`](archive/implementation-plan-first-release.md)).

The plan is expressed as dependency-ordered workstreams, not schedules.
Workstreams identify scope, deliverables, dependencies, and exit criteria so that
status can move from `Planned` to `Complete` when the criteria are met. No
workstream is considered done until its evidence is recorded in
[`docs/design/conformance.md`](design/conformance.md).

## How to read this plan

- **Status values:** `Complete`, `In progress`, `Planned`, `Blocked`.
- **Blocked** means an open design or product decision, not missing
  implementation capacity. Blocking decisions are listed under
  [Decision gates](#decision-gates).
- **Dependencies** are hard prerequisites. A workstream may not be marked
  `In progress` until each dependency is `Complete`.
- **Exit criteria** are verification-based. A workstream is `Complete` only when
  every requirement it introduces is covered by a test or by an explicit,
  documented deferral in the conformance matrix.
- Every new endpoint, status code, header, and validation branch must be added
  to `docs/design/conformance.md` in the same change that introduces it.

## What "complete conformance" means here

The specification separates three classes of work:

1. **Applicable MUST requirements** for the surface the framework exposes.
   These are correctness obligations and are always in scope.
2. **Optional base surface** (`self` links, related resource routes, pagination
   links, sorting on collections, filtering, profiles). The specification
   permits omission but requires correct rejection when unsupported. Whether to
   implement an optional feature is a product decision recorded in the
   conformance matrix.
3. **Association and persistence breadth.** Not a protocol requirement, but
   required for an application to express every base and Atomic operation for an
   arbitrary schema.

A conformance claim is the set of (endpoint, requirement) pairs the framework
asserts, with evidence. Until an optional feature is implemented, the framework
must reject requests that depend on it with the status the specification
requires.

## Baseline: already covered

The first-release profile already satisfies the applicable MUSTs for the surface
listed below. These areas are not re-implemented; new work must preserve their
evidence.

- Document structure, resource/identifier identity, member-name rules,
  `@`-members, duplicate-member rejection, `data`/`errors` exclusivity, included
  full linkage (including the sparse-fieldset exception).
- Link object and error object structure, JSON Pointer syntax, HTTP status
  strings.
- Content negotiation for the parameters the endpoints accept, `Vary: Accept`.
- Collection and single-resource reads, includes, sparse fieldsets, the custom
  filter/sort/pagination grammar, structured 404s.
- Base resource POST/PATCH/DELETE and relationship-linkage GET/PATCH/POST/DELETE
  for the supported association shapes.
- Atomic request/result shapes, ordering, local IDs, transaction rollback,
  extension media-type enforcement.

`null` link values are permitted by JSON:API 1.1 and are intentionally accepted.

## Workstreams

### A. Base protocol closure

Scope: close every applicable MUST gap on the endpoints already exposed.

**A1 — Unified media negotiation.**
Generalize negotiation so it is not hardcoded per endpoint: parse `Content-Type`
and `Accept` once, enforce the `ext`/`profile` parameter rules, reject
unsupported extensions with `415`, and reject unacceptable representations with
`406`. Allow the Atomic endpoint to accept a bare JSON:API `Accept` while still
emitting `ext` on the response, per the extension's rules.

**A2 — Response envelope.**
Emit the optional top-level `jsonapi` object (version and applied `ext`/`profile`)
and centralize top-level `links`/`meta` assembly so handlers do not construct
documents ad hoc.

**A3 — Error documents.**
Provide optional error `links` (`about`, `type`), support emitting multiple
errors for one request, and install a scoped JSON:API `404`/`405` fallback so the
component router is conformant standalone. Audit every error path for agreement
between the HTTP status and `errors[].status`.

**A4 — Write status matrix.**
Make `202 Accepted` and the `200`/`204` alternatives selectable per operation;
audit `409` coverage for server-enforced constraints and client-ID conflicts;
add the `403` path for disallowed to-many replacement; keep `Location` consistent
with any `self` link the response advertises.

**A5 — Atomic protocol closure.**
Decide and enforce the empty-`atomic:operations` rule; require an updated
representation when an executor changes fields beyond those in the operation;
support the `204 No Content` alternative when all results are empty.

- Depends on: **PR2**, **PR4**, **PR5**.
- Progress:
  - **A1 complete** — base and Atomic endpoints share one negotiation module;
    the Atomic endpoint applies its extension even when the client does not
    require it through `Accept`.
  - **A2 closed** — the top-level `jsonapi` object is not emitted (decision).
    Capability advertisement remains via `Content-Type`, which is sufficient
    because base endpoints apply no extension or profile.
  - **A3 partial** — `ApiBuilder::jsonapi_fallback` installs a scoped JSON:API
    `404`/`405` fallback. Optional error `links` (`about`, `type`) are deferred.
  - **A4 closed** — current write statuses (`201`/`200`/`204`) are retained;
    `202` is async-only and does not apply. `409`/`403` behavior is already
    covered; `Location`/`self` consistency is deferred to B1 (no `self` links
    exist yet).
  - **A5 complete** — empty `atomic:operations` is rejected with `400`; typed
    SeaORM add/update results return a resource representation (attributes and
    to-one relationships; to-many linkage is omitted because the standard
    mutation mapper cannot load it). The `204`-when-all-empty alternative is
    not adopted.
- Exit criteria met except the deferred optional items noted above; record
  further evidence in the conformance matrix as they land.

### B. Optional fetch surface

Scope: implement the optional read features consumers and conformance suites
exercise, and reject them correctly where not implemented.

**B1 — URL and link builder.**
Introduce a configurable base URL and mount prefix, and a single link builder
used by `Location`, `self`, `related`, and pagination links.

**B2 — `self` links.**
Emit document-level and resource-level `self` links; ensure a resource `self`
equals `Location` on create.

**B3 — Related resource routes.**
Serve `GET /{type}/{id}/{relationship}` for to-one (`null` when empty) and
to-many collections, honoring projection, authorization, limits, and 404
semantics.

**B4 — Relationship endpoint queries.**
Support `include` and sparse fieldsets on linkage endpoints by reusing the read
plan and projection.

**B5 — Pagination links.**
Emit `first`/`last`/`prev`/`next` (omitted or `null` when unavailable) using the
chosen pagination strategy contract.

**B6 — Related and relationship collection queries.**
Extend sorting, pagination, and filtering parity to related-resource and
relationship collections.

- Depends on: **PR1** (link builder), **A1**, **A3**.
- Progress: **B1/B2/B3/B5 partial** — opt-in `ApiBuilder::links` emits document
  `self`, resource `self`, and pagination (`first`/`prev`/`next`/`last`)
  links. Related-resource `GET /{type}/{id}/{relationship}` is served when the
  relationship grants `RelationshipPermission::RelatedRead` and a query adapter
  is configured. Related collections currently support includes and sparse
  fieldsets; filter/sort/page are rejected with `400` rather than ignored until
  B6 executes them. **B4 deferred** — `include`/`fields` on relationship
  endpoints are not supported and are rejected with `400`, which the
  specification permits for an endpoint that does not support `include`.
  **B6 deferred** — related collections reject filter/sort/page with `400`
  rather than supporting them. `related` links remain deferred until B3
  coverage is complete on both backends.
- Exit criteria: links resolve to working GETs; relationship and related
  endpoints pass include/fieldset/sort/page fixtures on PostgreSQL and SQLite;
  unsupported query controls are rejected with the specified status.

### C. Extensions and profiles

**Status: out of scope (decision).** Extensions beyond Atomic Operations are
not supported, and profiles are not applied. The framework continues to reject
unsupported extensions with `415`/`406` and to ignore unrecognized profiles as
the specification requires. C1–C3 are closed rather than planned; re-open this
workstream only if a concrete extension or profile is required.

Scope: implement the extension and profile mechanisms the base specification
defines, with Atomic as the first extension.

**C1 — Extension registry.**
Register supported extensions by URI, negotiate them through `ext`, apply them,
and advertise applied extensions in `Content-Type` and `jsonapi.ext`. Reject
unsupported extensions with the specified status.

**C2 — Profile framework.**
Register profiles, apply requested recognized profiles, ignore unknown profiles,
and advertise applied profiles in `Content-Type` and `jsonapi.profile`.

**C3 — Profile catalog.**
Each supported profile is its own requirement set, added to the conformance
matrix with its own tests. This workstream is `Blocked` until the supported
profile list is decided.

- Depends on: **A1**, **A2**, **PR2**.
- Exit criteria (C1, C2): extension/profile negotiation and advertising have
  positive and negative tests; Atomic is re-expressed through the registry.
  C3 is complete per profile once that profile's requirements are covered.

### D. Association and persistence breadth

Scope: replace the fixed storage shapes with an abstraction that can express
every base and Atomic operation for an arbitrary schema.

**D1 — Generic relationship access.**
Introduce a relationship access abstraction used by reads and mutations
(including Atomic), and express the current `ToOneForeignKey`,
`ToManyForeignKey`, and `JoinTable` handlers in terms of it.

**D2 — Required storage shapes.**
Support non-nullable direct foreign keys and join tables with additional
required columns, including their create/delete semantics for add, remove, and
replace.

**D3 — Ordered relationships.**
Support position columns, ordered linkage, and reordering for both base and
Atomic to-many operations.

**D4 — Other association shapes.**
Polymorphic and other ORM shapes, either through the abstraction or through
documented application executors. Scope is `Blocked` until target shapes are
enumerated.

- Depends on: **PR3**, **A5**.
- Progress: **D2 and D3 partial** — `SeaOrmJoinTableMutationHandler::new_with_insert_columns`
  lets applications populate additional required join-table columns on each
  inserted membership row, and the nullable-FK handler now also accepts a
  non-nullable foreign key for add/transfer while rejecting remove and replace.
  Ordered join tables (`RelationshipStorage::OrderedJoinTable`) preserve member
  order through a position column on replacement and append, and return linkage
  in position order. **D1 deferred** — no generic relationship-access refactor is
  required by the supported shapes. **D4 deferred** — polymorphic and other
  association shapes remain application-executor territory, matching the
  documented boundary.
- Exit criteria: each supported shape has read and write fixtures, idempotency
  and rollback evidence, and parity between PostgreSQL and SQLite; unsupported
  shapes are rejected or delegated explicitly.

### E. Verification

Scope: make conformance evidence systematic rather than per-feature.

**E1 — Requirement matrix expansion.** Extend
`docs/design/conformance.md` to enumerate the requirement classes in this plan
with representative evidence and explicit deferrals.

**E2 — External suite adoption.** Select a community JSON:API conformance suite,
run it against the exposed surface, and convert its failures into tracked
requirements or documented deferrals.

**E3 — Backend parity.** Keep the shared PostgreSQL/SQLite fixtures as the
definition of core parity and extend them with every new workstream.

**E4 — Parser and negotiation regressions.** Maintain structural-depth and
malformed-input regression coverage for filters, includes, documents, and
negotiation.

- Depends on: all workstreams.
- Progress: E1, E3, and E4 complete — the matrix records evidence for every
  implemented behavior, the PostgreSQL/SQLite shared fixtures define parity, and
  structural-depth/malformed-input regressions are covered. **E2 deferred** — no
  external conformance suite has been adopted; the matrix remains the evidence
  index, and adopting a suite is follow-up work outside the current decisions.
- Exit criteria: the matrix has no `Covered` claim without a test; the external
  suite result is recorded; parity fixtures cover each implemented shape.

## Architectural prerequisites

These unblock multiple workstreams and should precede feature work that depends
on them.

- **PR1 — URL/link builder** (configurable base URL, mount prefix). Unblocks B.
- **PR2 — Negotiation and envelope pipeline** (one media-type parser, one
  response assembler). Unblocks A1, A2, C. The media-type parser is complete;
  the response assembler remains planned.
- **PR3 — Generic relationship access trait.** Unblocks D.
- **PR4 — Executor "changed beyond request" signal.** Unblocks A5 and the update
  representation MUSTs.
- **PR5 — Scoped router fallback.** Unblocks A3.

## Milestones

| Milestone | Status | Depends on | Exit criteria |
| --- | --- | --- | --- |
| Phase 0 — Base protocol closure | Complete | PR2, PR4, PR5 | Workstream A met; optional error `links` deferred |
| Phase 1 — Fetch surface | Complete | PR1, Phase 0 | Workstream B met; B4/B6 deferred with conformant rejection |
| Phase 2 — Extensions and profiles | Closed | — | Out of scope per decision; Atomic remains the only supported extension |
| Phase 3 — Association breadth | Complete | PR3, Phase 0 | Workstream D met; D1/D4 deferred to application executors |
| Phase 4 — Verification closure | Complete | Phase 0–3 | Workstream E met; E2 external suite deferred |

## Decision gates

Open decisions that block named workstreams. Resolved decisions are recorded
here so scope does not silently change.

Open:

- None. Re-open a gate if new scope requires it.

Resolved:

- **Pagination strategy contract** — offset-based `page[number]`/`page[size]`
  links without a total count: `first`/`prev`/`next` are emitted when derivable
  and `last` is `null`. Related-collection queries (B6) are deferred.
- **Target association shapes** — deferred to application executors; ordered
  join tables and extra-column join tables are the built-in support boundary.
- **Extensions and profiles beyond Atomic** — out of scope. Atomic remains the
  only supported extension; profiles are not applied (unrecognized profiles are
  ignored).

- **Top-level `jsonapi` object** — not emitted. Capability advertisement uses
  `Content-Type`.
- **Empty `atomic:operations`** — rejected with `400`.
- **Write statuses** — retain `201`/`200`/`204`; `202` is async-only.
- **Atomic add/update results** — typed SeaORM results return a resource
  representation (attributes and to-one; to-many omitted). The `204`-when-empty
  alternative is not adopted.

## Relationship to other documents

- [`docs/design/conformance.md`](design/conformance.md) is the requirement-to-test
  index and the single source of conformance evidence. This plan states intent;
  the matrix states proof.
- [`docs/design/architecture.md`](design/architecture.md),
  [`docs/design/queries-and-includes.md`](design/queries-and-includes.md),
  [`docs/design/mutations-and-atomic-operations.md`](design/mutations-and-atomic-operations.md),
  and [`docs/design/resources-and-mapping.md`](design/resources-and-mapping.md)
  describe the current implemented boundaries; update them as workstreams land.
- [`docs/archive/implementation-plan-first-release.md`](archive/implementation-plan-first-release.md)
  preserves the first-release scope and release gate.

## Maintenance

Update this plan only when workstream scope, dependencies, or status change.
Record per-requirement evidence in `docs/design/conformance.md`. Do not restate
the conformance matrix here.
