# pbi-rs

`pbi-rs` searches source in the current repository with a bounded native walker. `--bm25` returns raw ranked hits and does not invoke a model.

Bare positional questions and `--message` synthesize answers from verified source evidence when a discovered config selects an approved local route and an approved credential environment handle is available. `PBI_RS_ADK_ENABLE=0` explicitly disables model use; without a configured route, the CLI emits deterministic evidence. The CLI validates model citations against bounded in-root source spans and shares one deadline across retrieval and synthesis. Explicit `--model-route` candidates require `PBI_RS_ADK_ENABLE=1` when no config is discovered.

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
