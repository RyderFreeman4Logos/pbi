# ADK model seam

The semantic slice is deterministic-first. Probe/BM25 runs first and the verifier must admit at least one bounded, in-root source span before a model binding can be used. Complete deterministic coverage returns directly without a model call.

For an incomplete `--message` query, `PBI_RS_ADK_ENABLE=1` opts into one supported local binding. The caller reuses `workflow-adk`'s existing `ModelProfileRegistry`, `OpenAiCompatibleProfile`, `ModelBinding`, `PromptProtocol`, `StructuredOutputContract`, and `ModelInvocationSpec`; it does not add a model client or copy the kit's route/hot-reload logic.

The current single-binding contract is:

1. Admit only the existing approved local route/model allowlist before resolving an existing environment credential handle. Unsupported cloud routes, models, incomplete opt-in configuration, and missing handles fail closed.
2. Build one bounded prompt from verified evidence, query, and missing target labels. No repository write or arbitrary model tool is available in this semantic slice; Probe remains the bounded read-only retrieval tool.
3. Pass one absolute deadline and cancellation flag through the invocation. Kit binding timeout remains an inner guard; the caller deadline is the outer boundary.
4. Require structured answer, explicit uncertainty, and at least one citation. A citation must exactly match a verifier-approved relative path and line span; fabricated, traversal, out-of-root, duplicate, or malformed citations are rejected.
5. Render only the model answer plus the exact source snippets already held by the verifier. Provider failures and invalid output do not become a success or a deterministic claim.

The kit still has no ordered model-route chain or model-configuration watcher. Ordered local-first fallback, last-valid hot reload, and full historical PBI parity remain explicit gaps for later milestones.
