# ADK model seam

## Dispatch contract

Native bounded BM25 runs first; source verification must admit bounded, in-root evidence before semantic invocation. `search` and raw BM25 are deterministic-only and never construct a semantic route. Bare positional questions and `--message` synthesize from verified evidence when opted in, including complete target coverage. Coverage completeness is not answer completion. With opt-in off they print verified evidence; an enabled invalid route or invalid model output fails closed.

## Pinned route boundary

All three manifest Kit edges and the five locked workflow packages use delivered `87ea1f3cff5e9c97b83fcf75841c62f6ac1bbfa4`. The SDK umbrella is `adk-rust =2.3.0`; consumer-root patches for adk-core, adk-graph, adk-action and adk-model reproduce Kit's delivered fork revision `1b73faf4f06004e028034e617270fc3003e75cad`. Dependency workspace patches do not propagate. The action patch remains unused while its optional feature is disabled.

`src/semantic.rs` builds a registry, ordered worker candidates, explicit authorization, one snapshot and publisher. Repeated leading `--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>` options remain authenticated candidates. Every candidate is validated before any profile is constructed, and the Kit maximum of eight candidates is enforced. Both public answer modes capture one frozen policy/snapshot and one absolute deadline through retrieval, planning and synthesis; cancellation and terminal-error behavior remain Kit-owned.

## Configuration and explicit NoAuth

An explicit `PBI_CONFIG_FILE`, otherwise XDG_CONFIG_HOME/pbi/config.toml or HOME/.config/pbi/config.toml, enables an approved configured route unless `PBI_RS_ADK_ENABLE=0`. With no discovered config, `PBI_RS_ADK_ENABLE=1` is required to opt in. An empty explicit PBI_CONFIG_FILE suppresses discovery. The CLI never loads `.env`, `api_key` or `key` configuration values. It is one-shot and has no reload daemon.

The config is opened nonblocking as a regular file and read at most 64 KiB. It selects `primary_model`, otherwise `model`, then the first matching `[[endpoints]]` entry. Route base/model fields use explicit CLI candidates first, then their existing environment aliases, then config/defaults. Complete base/model environment fields bypass file parsing and retain Credential. A partial model selects its matching endpoint; a partial base retains the configured primary model, and can retain NoAuth only for that exact configured base/model pair. A changed endpoint never borrows another endpoint's policy.

Only `auth = "no_auth"` explicitly selects NoAuth; omission retains the existing credential-handle selection and nonempty credential requirement. Unknown or nonstring auth values fail closed. CLI route candidates remain authenticated even when the config selects NoAuth. Every selected base/model still passes the unchanged allowlists: gb10:18009 or localhost:18317 and the three approved abliterated-qwen model identifiers. URL credentials, cloud targets, port8317 and arbitrary models do not become authorized by NoAuth.

PBI uses Kit's `OpenAiCompatibleAuth` and `OpenAiCompatibleProfile::new_no_auth`; it adds no transport or broker shim and supplies no placeholder token. Kit skips credential resolution and omits Authorization. NoAuth profile identity uses version 2; unchanged Credential profiles retain version 1. `--debug-config` reports only public route fields, the supported auth policy and redacted credential diagnostics without constructing a model or resolving a secret.

## Evidence and acceptance limits

Only verifier-approved evidence and missing targets enter the prompt: at most eight evidence items, 24 KiB rendered context and 16 KiB structured output. The response must contain an answer, uncertainty and citations matching admitted relative paths and line spans. Fabricated/outside/duplicate citations, forged body locations and invalid output publish no answer. Structured JSON Schema does not replace source validation. Public `--json` remains response text with sessionId/tokenUsage null.

Hermetic tests exercise both CLI forms through the real production NoAuth registry before replacing provider I/O, preserving the credential binding boundary. The consumer's resolved SDK also sends a bounded request to a test-owned random loopback listener which asserts the exact model and absent Authorization. Existing credential constructors, config precedence, all-candidate admission, deadline/cancellation and citation controls remain covered. This does not prove inference against either approved live endpoint, release/PATH installation identity, multi-route live fallback, or full historical PBI parity. Those acceptance rows remain pending until separately recorded real runs on the admitted endpoint and exact consumer artifact.
