# pbi

`pbi-rs` is the native binary. `just install-release <dir>` copies that same binary as `pbi-rs` and `pbi` into a caller-owned directory. It does not replace `/usr/local/bin/pbi`.

`pbi-rs search` prints compact source locations and Okapi BM25 scores; `--bm25` adds source blocks from the same bounded native ranker. Both evaluate uppercase Boolean AND/OR/NOT, parentheses, and quoted phrases without a syntax flag. Ordinary keyword bags remain supported. `--strict-elastic-syntax` retains its additional legacy admission rules; `--exact` retains literal matching. Search hits are not semantic citations, and neither search mode invokes a model. The historical Probe shell wrapper is archived under `archive/shell/` and is not the default command.

Bare positional questions and `--message` synthesize answers from verified source evidence when a discovered config selects an approved local route and an approved credential environment handle is available. `PBI_RS_ADK_ENABLE=0` explicitly disables model use; without a configured route, the CLI emits deterministic evidence. The CLI validates model citations against bounded in-root source spans and shares one deadline across retrieval and synthesis. Explicit `--model-route` candidates require `PBI_RS_ADK_ENABLE=1` when no config is discovered.

## Source extraction

`pbi-rs extract <path>:<line> [--timeout <SECONDS>] [--max-bytes <N>]` returns the smallest enclosing Rust item (including attributes/doc comments and nested functions), using the existing `syn` parser. Locations outside an item and non-Rust files return an explicitly approximate four-line neighbor window. Output carries the source span; oversized blocks say `Block: truncated` and end with `[truncated]`. The cap includes headers and is at most 32 KiB; caps too small for the header/marker fail closed.

Paths are relative to the current project root, or absolute inside that root. Parent traversal, ignored/hidden files, symlink components, non-regular files, other devices, files over 2 MiB, invalid positions and expired deadlines are refused without echoing private input. Extraction shares search's bounded ignore-aware walk and 16-root-target ceiling. No model, Probe, shell search, new dependency, or repository write is involved.

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
