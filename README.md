# pbi-rs

`pbi-rs` is the first replacement slice for PBI: it delegates BM25 retrieval to the installed Probe binary and emits only source-verified locations inside the current repository. `--bm25` is an explicit raw-retrieval escape hatch and does not invoke a model.

Bare positional questions and `--message` request answer synthesis and are evidence-first: Probe evidence is verified before any semantic call, but complete coverage does not bypass synthesis. When the report has verified evidence and `PBI_RS_ADK_ENABLE=1`, the CLI builds a single authorized local `workflow-adk` `ModelRouteSnapshot`, captures one `ModelRoutePolicy` with the shared absolute deadline, and invokes it with a cancellation token. Local route/model admission precedes profile construction. Only bounded verified evidence reaches the model; structured answer/uncertainty citations must exactly match verified in-root spans. Without explicit opt-in, the CLI emits deterministic evidence. The kit's ordered fallback is exercised with offline fake adapters, but the CLI configures only one route; multi-route configuration, daemon reload, approved-local live inference, and full parity are not claimed.

## Local commands

`--debug-config` intentionally hardens privacy: any configured `PBI_RS_PROBE` is shown as `probe_binary=[REDACTED]`, never its path (even escaped); only an unset override shows `probe_binary=probe`. This diagnostic-only compatibility change does not alter Probe selection or execution.

All Cargo commands run through `just` so the repository's idle-I/O and canonical SSD target rules remain active:

```text
just fmt
just test
just build
just clippy
just gate
```
