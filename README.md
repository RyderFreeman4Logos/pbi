# pbi

`pbi-rs` is the native binary. `just install-release <dir>` copies that same binary as `pbi-rs` and `pbi` into a caller-owned directory. It does not replace `/usr/local/bin/pbi`.

`pbi-rs search` prints compact source locations and Okapi BM25 scores; `--bm25` adds source blocks from the same bounded native ranker. Both evaluate uppercase Boolean AND/OR/NOT, parentheses, and quoted phrases without a syntax flag. Ordinary keyword bags remain supported. `--strict-elastic-syntax` retains its additional legacy admission rules; `--exact` retains literal matching. Search hits are not semantic citations, and neither search mode invokes a model. The historical Probe shell wrapper is archived under `archive/shell/` and is not the default command.

Bare positional questions and `--message` synthesize answers from verified source evidence when a discovered config selects an approved local route and an approved credential environment handle is available. `PBI_RS_ADK_ENABLE=0` explicitly disables model use; without a configured route, the CLI emits deterministic evidence. The CLI validates model citations against bounded in-root source spans and shares one deadline across retrieval and synthesis. Explicit `--model-route` candidates require `PBI_RS_ADK_ENABLE=1` when no config is discovered.

## Local commands

`--debug-config` reports `search_default=native_bounded_bm25_compact_no_probe`. Probe overrides are ignored and never printed; credential values are redacted.

All Cargo commands run through `just` so the repository's idle-I/O and canonical SSD target rules remain active:

```text
just fmt
just test
just build
just clippy
just gate
```
