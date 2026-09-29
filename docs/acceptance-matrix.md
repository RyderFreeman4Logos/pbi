# Replacement acceptance matrix

Status values are **PASS** only after the command or test has actually run in this checkout. **GAP** means intentionally outside this milestone; it is not replacement-ready.

## Existing CLI surface

| Contract from `pbi` | pbi-rs milestone behavior | Status |
|---|---|---|
| `pbi <question...>` | Runs Probe BM25 first and prints compact verified evidence: exact source spans, target, relevance, and optional symbol. | PASS after `just acceptance` |
| `pbi search <query>` | Same deterministic compact evidence path; incomplete target coverage is reported instead of claimed complete. | PASS after `just acceptance` |
| `pbi search --bm25 <query>` | Relays raw Probe stdout/stderr; no model call. | PASS after `just acceptance` |
| `pbi --message <question>` | Accepted as a deterministic search alias; semantic model answer is not claimed. | PARTIAL |
| `pbi --debug-config` | Safe diagnostic output; auth is always `[REDACTED]`. | PASS after `just acceptance` |
| `--timeout`, `--max-results` | Validated and passed as Probe arguments. | PASS after `just acceptance` |
| Probe option pass-through (`--reranker`, `--session`, `--format`, etc.) | Deliberately rejected until the compatibility surface is specified. | GAP |
| `--json` output | Not implemented; no JSON contract is asserted. | GAP |
| Interactive mode | Remains disabled; missing question returns status 2. | PASS by CLI parser test/manual check |

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
| ADK model invocation | Kit exposes `OpenAiCompatibleProfile`, `ModelProfileRegistry`, `ModelBinding`, and bounded per-binding calls. | GAP: add the semantic caller through those primitives; do not add a second client |
| Ordered local-first fallback | Kit registry is role-based, not an ordered route chain; one binding retries without switching endpoints. | GAP: dependency seam is an immutable ordered route-chain snapshot |
| Explicitly authorized routes | Existing PBI has a narrow local route allowlist; kit validates URL shape only. | GAP: route admission policy must be explicit before binding |
| Shared absolute deadline/cancellation | Kit binding timeout starts per binding; this does not cover fallback. | GAP: pass one invocation deadline/cancel token through every attempt |
| Invalid hot reload retains last valid config | Kit hot reload covers development transform packages, not model route configuration. | GAP: atomic config publisher with last-valid retention and per-call snapshot |
| Source-cited model output | No semantic path in this slice. | GAP: structured ADK response must pass this verifier before emission |
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
