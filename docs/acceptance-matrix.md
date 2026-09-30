# Source-pinned minimal CLI parity matrix

Legacy source is read-only checkout `/home/obj/project/github/RyderFreeman4Logos/pbi`, HEAD `d239a1018cab3c441ca4ec7b394f7ec466442ee0`, tree `cd5115ef5dc9f810e9844266753fbfa7280467f3`. Its public help source (`pbi:23-27`) advertises `pbi <question...> [--json]`, `pbi search [--bm25] <query>`, `pbi --message <question>`, and `pbi --debug-config`; it states that normal `search` prints compact verified BM25 locations without chat and `--bm25` prints raw no-LLM output.

Parity status: **VERIFIED** = source-pinned behavior matches the current acceptance; **PARTIAL** = only the stated bounded slice is verified; **MISSING** = source-pinned behavior is not implemented; **UNVERIFIED** = evidence is blocked or absent. This table is the CLI parity claim; the following safety table retains its earlier PASS/GAP labels.

| Legacy contract and source pin | pbi-rs evidence | Status |
|---|---|---|
| Normal positional question synthesizes a cited answer (`pbi/test_pbi.py:681-723`). | `src/main.rs` dispatches bare questions through verified Probe → authorized ADK route policy → exact-citation validator under explicit model opt-in. A command-dispatch test uses fake Probe and fake model: exact `receipt.py:1` answer accepted; forged `../outside.py` rejected without output. | **PARTIAL** — hermetic synthesis verified; approved-local live binding/inference remains unverified. No-model fallback still returns verified evidence. |
| `pbi search <query>` is BM25-only/no-chat (help; `pbi:4585-4607`). | Fixture and real-repository search pass `just acceptance`; verified snippets and coverage are emitted. | **VERIFIED** |
| `pbi search --bm25 <query>` returns raw Probe output (help; `pbi:4562-4583`). | Raw fixture output is checked by `just acceptance`; `pbi-rs` writes raw streams without evidence parsing. | **VERIFIED** |
| No source locations: exit 1, empty stdout, exact `pbi: no source locations found` (`pbi:4617-4619`; `test_pbi.py:661-679`). | Regression in `scripts/acceptance.sh` asserts exit 1, empty stdout, exact stderr on the fixture. | **VERIFIED** |
| Search scope is the invocation directory (`pbi:4484`; help exposes no explicit `--file`/`--path`). | `pbi-rs` canonicalizes CWD and has a bounded 16-target fallback. Existing `probe-matrix.json` shows direct Probe root misses versus explicit `src/lib.rs` hits; real CWD search passes acceptance. | **PARTIAL** — CWD case verified; arbitrary per-file scope and fallback saturation are not. |
| `--timeout`, `--max-results` are accepted on legacy `search` (`pbi:4517-4531`). | `pbi-rs` parses and forwards these options; this acceptance does not assert child argv or legacy default parity. | **PARTIAL** |
| Legacy help advertises positional `--json` (`pbi:23`): bare questions remove it from the query and append it to chat arguments (`pbi:4781-4799`); model review/audit re-apply it (`pbi:5085-5125`). `--message` passes extra options through (`pbi:4761-4779`). `search --bm25` remains raw passthrough (`pbi:4562-4583`). Installed `@probelabs/probe-chat@0.6.0-rc330` (`/home/obj/.local/node_modules/@probelabs/probe-chat/package.json:2-3`) serializes successful non-interactive Chat replies as `{response, sessionId, tokenUsage}` (`index.js:417-435`), error JSON to stderr (`index.js:439-452`). Legacy fast paths can return without Chat (`pbi:4984-5012`), so they need not return this envelope. | Command-dispatch fake Probe + fake ADK route asserts positional and `--message --json` parse/escape a cited success in the Chat-shaped envelope; invalid citation and no-hit return no stdout. The ADK structured-output schema and exact-citation validator remain unchanged. `sessionId` is the ADK invocation identity, **not** Probe Chat's resumable session ID; `tokenUsage` is null because the validated ADK answer exposes no Chat usage. No-model BM25 fallback retains plain evidence and no-hit retains rc 1 with empty stdout. | **PARTIAL** — supported Chat-shaped success, escaping, invalid output, and no-hit verified; Probe's actual session continuity and token accounting are not reproducible without the Chat runtime, and legacy fast paths are not universally JSON. |
| `--message` semantic invocation. | Repeated leading `--model-route <BASE_URL> <MODEL> <CREDENTIAL_HANDLE_NAME>` options feed the same admission and dispatch path used by positional questions. Hermetic CLI tests inject fake ADK bindings; live profile binding was not performed. No credential/environment probes were made for this slice. | **PARTIAL** — offline only |

## Deterministic safety and evidence

| Acceptance | Evidence | Status |
|---|---|---|
| BM25 is mandatory before any semantic path | `src/main.rs` always invokes `probe search --reranker bm25`; no model dependency exists in this slice. | PASS |
| Bare location stamps fail closed | `verify_probe_evidence` requires readable in-root source text and a compact locally relevant span; `verify_probe_locations` remains the range-bounded compatibility view. | PASS by unit test |
| Unrelated spans fail closed | Focused fixtures reject lexical test/decoy spans, retain verified partial evidence, and emit explicit missing targets. | PASS after `just acceptance` |
| Source boundary is preserved | Canonical path must remain under the invocation root; outside-root candidates are discarded. | PASS by unit test |
| Generated/noise boundaries | `drafts`, `target`, `node_modules`, and `__pycache__` are excluded. | PASS by unit test coverage through verifier |
| Raw mode remains raw | `--bm25` does not parse, summarize, or invoke a model. | PASS after `just acceptance` |
| Secrets stay out of output | This code does not read `.env`; debug output emits only `[REDACTED]`. | PASS after `just acceptance`; external Probe policy remains inherited |

## Model and replacement gaps

| Requirement | Current live evidence | Status / next boundary |
|---|---|---|
| ADK model invocation | `ModelRoutePublisher` captures an authorized kit snapshot and policy; positional questions and `--message` invoke it behind explicit opt-in. | PARTIAL: live profile binding/inference has not been smoke-tested |
| Ordered local-first fallback | Repeated leading `--model-route` options configure ordered local candidates; CLI-path fake-adapter tests cover transient fallback and denial cases. | PARTIAL: hermetic dispatch is verified; live multi-route fallback is unverified |
| Explicitly authorized routes | Every candidate is allowlist-checked before profile construction; credential handle names are broker-resolved. CLI-path tests reject an unapproved later candidate before route construction. | PARTIAL: offline admission is verified; live multi-route binding is unverified |
| Shared absolute deadline/cancellation | One absolute deadline and frozen snapshot are passed through CLI policy and `investigate`; CLI-path pending-adapter tests verify timeout without fallback. | PARTIAL: offline timing is verified; live multi-route timing is unverified |
| Route reload | This is a one-shot CLI; configuration is read per invocation, with no daemon reload path. | NOT APPLICABLE: invoke again to read changed options; no hot-reload claim |
| Source-cited model output | Structured ADK output requires answer, explicit uncertainty, and exact citations matching verifier-approved in-root spans; fake tests reject fabricated citations. | PASS in offline tests |
| Full old PBI parity | Existing tests include fail-closed compression/FairLance/provider-location cases and route-denial behavior (historical source lines recorded in the feasibility report). | GAP: port each contract with sanitized fixtures; keep existing PBI as fallback |

## Commands

```text
just fmt
just test
just clippy
just gate
just acceptance
```

`just acceptance` runs a deterministic fake Probe fixture and a real Probe query against this repository. No cloud endpoint is configured or called.
