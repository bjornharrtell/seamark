# Architecture and request lifecycle

**Status: proposal for review.** This document describes a possible architecture, not implemented behavior.

## Boundaries

The proposed design separates protocol handling, API metadata, application policy, and persistence:

1. **Axum adapter** accepts HTTP requests, performs content negotiation, routes JSON:API endpoints, and writes protocol responses. Axum is the initial integration target, not a commitment to make HTTP concerns part of the persistence layer.
2. **Protocol layer** parses JSON:API documents and query parameters, applies structural and protocol validation, and serializes resource, relationship, error, and operation documents.
3. **Resource registry** describes public resource types, attributes, relationships, links, and permitted operations. It is separate from database schema discovery.
4. **Planning layer** resolves a request into validated, typed read or mutation plans, including requested fields, relationships, ordering, pagination, and negotiated extensions/profiles.
5. **Application hooks** allow validation, authorization, business rules, and link customization to participate at defined points. Framework defaults should not prevent application-specific rules.
6. **SeaORM adapter** executes planned persistence work and maps results to API-facing values. It is the initial persistence integration; a generic multi-ORM layer is not a requirement.

The exact module boundaries and public extension points remain open.

## Proposed request lifecycle

For a read request, the intended flow is:

1. Match an endpoint and establish request context.
2. Negotiate the JSON:API media type, extensions, and profiles according to the specification and endpoint capabilities.
3. Parse and validate the path, query string, and relevant headers into typed inputs.
4. Resolve the resource and relationship metadata, then authorize the requested operation and fields.
5. Build a typed query plan, enforcing supported semantics and configured cost/resource limits.
6. Execute database-side selection and relationship loading through SeaORM.
7. Build the response document, including requested relationships and links, and serialize it with correct protocol headers.

Mutation requests follow the same negotiation and validation boundaries, but produce explicit changesets or operation plans. Application validation and authorization should run before writes where possible; transactional execution, persistence results, and response serialization should have clear boundaries.

Malformed or unsupported requests should produce protocol-appropriate errors. The system must not turn unsupported query behavior into a successful but semantically different in-memory operation.

## Cross-cutting concerns

- **Negotiation:** Only extensions and profiles that the server implements and tests should be advertised or accepted as supported.
- **Errors:** Centralize conversion of parsing, validation, authorization, and persistence failures into JSON:API error documents without hiding useful application context.
- **Links:** Use a configurable link-generation boundary so deployments can account for prefixes, hosts, and route choices.
- **Limits:** Apply explicit request-body, include-depth, query-cost, and related resource limits.
- **Observability:** Keep errors and execution boundaries inspectable without leaking sensitive values into responses or logs.
- **Testing:** Make protocol behavior testable independently from database execution, and exercise the Axum integration through request/response tests.

## Decisions to resolve

- How much of the proposed layering should be public API versus internal implementation?
- Should standard routes be registered automatically from resource metadata, explicitly declared by applications, or supported in both modes?
- Which negotiation and endpoint-capability configuration belongs to the application versus the framework?
- What is the intended compatibility and stability policy for hooks and resource declarations?
