# JSON:API conformance strategy

**Status: proposal for review.** The repository does not yet contain an implementation or passing conformance suite.

## Compliance objective

The target protocol is JSON:API 1.1, with full normative compliance for the base format rather than a selected subset. Correct negotiation and implementation of any supported extensions and profiles are also required. Atomic Operations is an important extension goal, but is not an implemented feature. A server must not claim support for arbitrary third-party extensions or profiles merely because it can parse their names.

## Conformance ledger

Maintain a ledger that traces every applicable normative requirement to implementation evidence and automated tests. The ledger should be reviewable alongside code and test changes. For each requirement, record:

| Field | Purpose |
| --- | --- |
| Requirement ID and source | Stable reference to the JSON:API 1.1 clause, extension, or profile requirement |
| Applicability | Conditions under which the requirement applies, including negotiated capabilities |
| Behavior | Expected server behavior and relevant error/negotiation cases |
| Implementation location | Code path responsible for the behavior |
| Tests | Automated tests covering success, invalid input, and relevant edge cases |
| Status and gap | Not started, implemented, tested, or blocked, with an explanation for gaps |

Every normative requirement should have an explicit applicability decision; “not supported” is not an acceptable way to waive a base-format requirement. Extension/profile requirements apply when the capability is implemented and advertised, and their negotiation behavior must also be tested.

## Test strategy

The eventual suite should combine:

- Requirement-focused unit tests for parsing, validation, negotiation, and serialization.
- Protocol-level request/response tests for routes, headers, status codes, documents, and error behavior.
- Persistence integration tests for query translation, relationship loading, mutations, transaction rollback, and concurrency behavior.
- Extension/profile-specific suites that prove both advertised behavior and rejection/negotiation behavior when unsupported.
- Axum integration tests using realistic application resource declarations.

Tests should include negative cases and boundaries, not only happy-path examples. Query tests should verify database-side semantics and limits; mutation tests should verify rollback and ordering wherever promised.

## Release gates

A release that claims JSON:API 1.1 compliance should not ship while an applicable base normative requirement is unimplemented, untested, or silently approximated. Every advertised extension or profile must have its own complete ledger coverage and negotiation tests. Any known exception or limitation must be clearly scoped and must not be presented as full compliance.

Before implementation begins, the project should select an authoritative version of the specification and maintain the requirement inventory against it. Automated tests should be the primary evidence, with the ledger serving as a traceability and review index rather than a substitute for tests.
