# pbi-rs

`pbi-rs` is the first replacement slice for PBI: it delegates BM25 retrieval to the installed Probe binary and emits only source-verified locations inside the current repository. `--bm25` is an explicit raw-retrieval escape hatch and does not invoke a model.

`--message` is deterministic-first: Probe evidence is verified before any semantic call. When coverage is incomplete and `PBI_RS_ADK_ENABLE=1`, the single-binding ADK path uses the existing `workflow-adk` `ModelProfileRegistry`, `ModelBinding`, and `ModelInvocationSpec` interfaces. It admits only the existing approved local routes/models, sends only bounded verified evidence, and accepts only structured answer/uncertainty output whose citations exactly match verified in-root spans. Without explicit opt-in, it remains the deterministic alias; ordered fallback, hot reload, and full parity are not claimed.

## Local commands

All Cargo commands run through `just` so the repository's idle-I/O and canonical SSD target rules remain active:

```text
just fmt
just test
just build
just clippy
just gate
```
