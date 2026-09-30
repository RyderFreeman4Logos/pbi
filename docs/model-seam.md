# ADK model seam

## Dispatch contract

Probe/BM25 runs first; `verify_probe_evidence` must admit bounded, in-root source evidence before semantic invocation. `search` and raw BM25 are deterministic-only and never construct a semantic route. Bare positional questions and `--message` explicitly request an answer: when opted in, they synthesize from verified evidence even when target coverage is complete. Coverage completeness is not answer completion. With opt-in off, those modes print verified evidence and return success only for complete coverage; an enabled but invalid route or invalid model output fails closed.

## Pinned route boundary

`Cargo.lock` pins `workflow-adk` to `42b1834682e433c30e23527c9234c8acc821426b`. `src/semantic.rs` builds a `ModelProfileRegistry`, one worker `ModelRouteCandidate`, an explicit `ModelRouteAuthorization`, then a `ModelRouteSnapshot` and `ModelRoutePublisher`. `src/main.rs` captures one policy per semantic request and passes its absolute deadline and cancellation token through `investigate`; the CLI currently has one approved-local candidate, not ordered fallback or reload.

`PBI_RS_ADK_ENABLE=1` is required for semantic dispatch. Route/model values are allowlisted; credentials are resolved only through the kit `CredentialBroker`, with an optional source-approved handle name in `PBI_RS_CREDENTIAL_HANDLE`. Defaults and route overrides are declared in `src/semantic.rs`; this slice does not load `.env` or PBI TOML configuration.

## Evidence and answer contract

The prompt is built only from verifier-approved evidence and missing targets, bounded by `MAX_SEMANTIC_EVIDENCE`, `MAX_SEMANTIC_CONTEXT_BYTES`, and `MAX_SEMANTIC_OUTPUT_BYTES`. The structured response must contain an answer, uncertainty, and citations matching an approved relative path and exact verified line span. Fabricated or out-of-root citations and invalid output are rejected; a lexical coverage report alone is never emitted as a synthesized answer.

The positional/`--message` mode intentionally retains synthesis for complete evidence. Search/BM25 remains the no-model path. Live approved-route binding/inference has not been validated; ordered CLI fallback, route reload, PBI config-file parity, and full historical PBI parity remain out of scope.
