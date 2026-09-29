# ADK model seam

The semantic slice is deterministic-first. Probe/BM25 runs first and the verifier must admit at least one bounded, in-root source span before a model binding can be used. Complete deterministic coverage returns directly without a model call.

For an incomplete `--message` query, `PBI_RS_ADK_ENABLE=1` opts into one supported local binding. The caller reuses `workflow-adk`'s existing `ModelProfileRegistry`, `OpenAiCompatibleProfile`, `ModelBinding`, `PromptProtocol`, `StructuredOutputContract`, and `ModelInvocationSpec`; it does not add a model client or copy the kit's route/hot-reload logic.

The current single-binding contract is:

1. `PBI_RS_ADK_ENABLE=1` is a process-local opt-in. Route/model values may be supplied by the existing `CLIPROXY_BASE_URL`, `LOCAL_ROUTER_BASEURL`, `LOCAL_MODEL`, or `LLM_MODEL` overrides; otherwise pbi-rs uses the PBI source defaults `http://localhost:18317/v1` and `abliterated-qwen-latest-27b-none`. Every resolved value is checked against the existing local allowlists before credential resolution.
2. Credentials are resolved only through the kit `CredentialBroker`. Set process-local `PBI_RS_CREDENTIAL_HANDLE` to one source-approved handle name (`CLIPROXY_API_KEY`, `OPENAI_API_KEY`, or `LOCAL_ROUTER_API_KEY`) to bind that handle directly without caller-side secret inspection; when unset, the legacy first-nonempty PBI order applies. Diagnostics list handle names only. pbi-rs does not read `.env` or PBI's TOML config; config-file parity is not part of this slice.
3. Each invocation captures one immutable `ModelBinding` from the resolved route/model/credential handle. Unsupported cloud routes/models, invalid opt-in, and missing credential handles fail closed; no unauthenticated or guessed-credential path is added.
4. Build one bounded prompt from verified evidence, query, and missing target labels. No repository write or arbitrary model tool is available in this semantic slice; Probe remains the bounded read-only retrieval tool.
5. Pass one absolute deadline and cancellation flag through the invocation. Kit binding timeout remains an inner guard; the caller deadline is the outer boundary.
6. Require structured answer, explicit uncertainty, and at least one citation. A citation must exactly match a verifier-approved relative path and line span; fabricated, traversal, out-of-root, duplicate, or malformed citations are rejected.
7. Render only the model answer plus the exact source snippets already held by the verifier. The CLI emits the ADK invocation identity as a model-stage attestation. Provider failures and invalid output do not become a success or a deterministic claim.

The pbi-rs lockfile still pins `workflow-adk` commit `41722ebac602639baad97e36170b83135172b766`, which has no ordered route snapshot API. The shared kit checkout has a committed but unmerged `ModelRouteSnapshot` implementation (`4c2a9f16d167cda02bd948e8f0ae8242fed86fa3`) and a later test commit (`207e39afa04eba6d7f4002d32c00bdac86feea0a`); its branch is local-only and its working tree is dirty, so pbi-rs does not pin or consume it. No kit tests were run from this shared checkout. Ordered local-first fallback, last-valid hot reload, PBI TOML config loading, and full historical PBI parity remain explicit follow-on gaps.
