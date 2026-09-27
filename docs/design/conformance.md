# JSON:API conformance

**Status: proposal for review.** No implementation or conformance suite exists yet.

The initial complete-release objective is full normative compliance with the JSON:API 1.1 base specification and complete support for its Atomic Operations extension. Other third-party extensions and profiles are deferred.

Tests should provide evidence for normative protocol behavior, including negotiation, valid and invalid requests, response documents, and extension behavior. Query tests should verify the documented server filter and pagination contracts; mutation tests should cover ordered Atomic Operations, local-ID references, transaction rollback, and the absence of partial success.

This objective describes intended scope only. It must not be read as a claim that Seamark currently implements any JSON:API behavior.
