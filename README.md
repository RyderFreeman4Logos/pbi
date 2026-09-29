# pbi-rs

`pbi-rs` is the first replacement slice for PBI: it delegates BM25 retrieval to the installed Probe binary and emits only source-verified locations inside the current repository. `--bm25` is an explicit raw-retrieval escape hatch and does not invoke a model.

The semantic model path is intentionally not implemented in this milestone. Its integration boundary is documented in `docs/model-seam.md`; the existing PBI remains the fallback.

## Local commands

All Cargo commands run through `just` so the repository's idle-I/O and canonical SSD target rules remain active:

```text
just fmt
just test
just build
just clippy
just gate
```
