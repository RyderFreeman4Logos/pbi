# Source-pinned minimal CLI parity matrix

Legacy source is read-only checkout `/home/obj/project/github/RyderFreeman4Logos/pbi`, HEAD `d239a1018cab3c441ca4ec7b394f7ec466442ee0`, tree `cd5115ef5dc9f810e9844266753fbfa7280467f3`. Its public help source (`pbi:23-27`) advertises `pbi <question...> [--json]`, `pbi search [--bm25] <query>`, `pbi --message <question>`, and `pbi --debug-config`; it states that normal `search` prints compact verified BM25 locations without chat and `--bm25` prints raw no-LLM output.

Parity status: **VERIFIED** = source-pinned behavior matches the current acceptance; **PARTIAL** = only the stated bounded slice is verified; **MISSING** = source-pinned behavior is not implemented; **UNVERIFIED** = evidence is blocked or absent. This table is the CLI parity claim; the following safety table retains its earlier PASS/GAP labels.

| Legacy contract and source pin | pbi-rs evidence | Status |
|---|---|---|
| Normal positional question synthesizes a cited answer (`pbi/test_pbi.py:681-723`). | `src/main.rs` dispatches bare questions through the same verified Probe → ADK binding → exact-citation validator as `--message` under explicit model opt-in. A command-dispatch test uses fake Probe and test-only ADK binding: exact `receipt.py:1` answer accepted; forged `../outside.py` rejected without output. | **PARTIAL** — hermetic synthesis verified; approved-local live model binding/inference remains unverified. No-model fallback still returns verified evidence. |
| `pbi search <query>` is BM25-only/no-chat (help; `pbi:4585-4607`). | Fixture and real-repository search pass `just acceptance`; verified snippets and coverage are emitted. | **VERIFIED** |
| `pbi search --bm25 <query>` returns raw Probe output (help; `pbi:4562-4583`). | Raw fixture output is checked by `just acceptance`; `pbi-rs` writes raw streams without evidence parsing. | **VERIFIED** |
| No source locations: exit 1, empty stdout, exact `pbi: no source locations found` (`pbi:4617-4619`; `test_pbi.py:661-679`). | Regression in `scripts/acceptance.sh` asserts exit 1, empty stdout, exact stderr on the fixture. | **VERIFIED** |
| Search scope is the invocation directory (`pbi:4484`; help exposes no explicit `--file`/`--path`). | `pbi-rs` canonicalizes CWD and has a bounded 16-target fallback. Existing `probe-matrix.json` shows direct Probe root misses versus explicit `src/lib.rs` hits; real CWD search passes acceptance. | **PARTIAL** — CWD case verified; arbitrary per-file scope and fallback saturation are not. |
| `--timeout`, `--max-results` are accepted on legacy `search` (`pbi:4517-4531`). | `pbi-rs` parses and forwards these options; this acceptance does not assert child argv or legacy default parity. | **PARTIAL** |
| `--json` is advertised by the legacy help. | No JSON answer/output contract is implemented or tested in this slice. | **MISSING** |
| `--message` semantic invocation. | Fake route/binding tests exist; live profile-binding smoke remains blocked. No new credential or environment probes were made for this slice. | **UNVERIFIED** |

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
| ADK model invocation | Fake binding tests exercise `ModelProfileRegistry` → `ModelBinding` → `ModelInvocationSpec`; `--message` wires the same path behind explicit opt-in. | PARTIAL: single binding only; no approved-local real model smoke recorded yet |
| Ordered local-first fallback | Kit registry is role-based, not an ordered route chain; one binding retries without switching endpoints. | GAP: dependency seam is an immutable ordered route-chain snapshot |
| Explicitly authorized routes | Single-binding opt-in admits only the existing approved local route/model allowlist before credential resolution; unsupported routes fail closed. | PASS for this slice; ordered fallback admission remains GAP |
| Shared absolute deadline/cancellation | The CLI creates one absolute Probe/model deadline; semantic invocation also accepts cancellation and fails closed before/within the call. | PASS for single binding; shared fallback budget remains GAP |
| Invalid hot reload retains last valid config | Kit hot reload covers development transform packages, not model route configuration. | GAP: atomic config publisher with last-valid retention and per-call snapshot |
| Source-cited model output | Structured ADK output requires answer, explicit uncertainty, and exact citations matching verifier-approved in-root spans; fake tests reject fabricated citations. | PASS for single binding |
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
