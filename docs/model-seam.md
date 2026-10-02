# ADK model seam

## Dispatch contract

Probe/BM25 runs first; `verify_probe_evidence` must admit bounded, in-root source evidence before semantic invocation. `search` and raw BM25 are deterministic-only and never construct a semantic route. Bare positional questions and `--message` explicitly request an answer: when opted in, they synthesize from verified evidence even when target coverage is complete. Coverage completeness is not answer completion. With opt-in off, those modes print verified evidence and return success only for complete coverage; an enabled but invalid route or invalid model output fails closed.

## Pinned route boundary

`Cargo.lock` pins `workflow-adk` and `workflow-runtime` to `a8d5c826bd58b22406d4f5d4f2266ca1c9f702a1`. `src/semantic.rs` builds a `ModelProfileRegistry`, ordered worker `ModelRouteCandidate`s, explicit `ModelRouteAuthorization`, then a `ModelRouteSnapshot` and `ModelRoutePublisher`. Repeated leading `--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>` options configure the ordered candidates for a semantic request; every candidate is validated before any profile is constructed, and the kit maximum of eight candidates is enforced. Without route options the existing single-route default remains in effect. `src/main.rs` captures one frozen policy/snapshot and one absolute deadline per request and passes the same deadline and cancellation token through `investigate`.

`PBI_RS_ADK_ENABLE=1` is required for semantic dispatch. Route/model values are allowlisted; CLI configuration accepts credential handle names only, resolved by the existing kit `CredentialBroker`. The single-route default may use the existing source-approved `PBI_RS_CREDENTIAL_HANDLE`. Defaults and route overrides are declared in `src/semantic.rs`; this slice does not load `.env` or PBI TOML configuration. The CLI is one-shot and reads options anew on the next invocation; it has no reload daemon.

## Evidence and answer contract

The prompt is built only from verifier-approved evidence and missing targets, bounded by `MAX_SEMANTIC_EVIDENCE`, `MAX_SEMANTIC_CONTEXT_BYTES`, and `MAX_SEMANTIC_OUTPUT_BYTES`. The structured response must contain an answer, uncertainty, and citations matching an approved relative path and exact verified line span. Fabricated or out-of-root citations and invalid output are rejected; a lexical coverage report alone is never emitted as a synthesized answer.

The positional/`--message` mode intentionally retains synthesis for complete evidence. Search/BM25 remains the no-model path. Hermetic CLI-path tests inject fake adapters to verify ordered transient fallback, preconstruction rejection, nonretryable denial, and one total deadline; they do not establish live binding or inference. Live approved-route binding/inference has not been validated; PBI config-file parity and full historical PBI parity remain out of scope.
