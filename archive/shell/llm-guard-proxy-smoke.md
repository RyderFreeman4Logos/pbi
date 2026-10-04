# llm-guard-proxy frozen smoke receipt

- Date (UTC): 2026-08-30T23:49:41Z
- Origin: `https://github.com/RyderFreeman4Logos/llm-guard-proxy.git`
- Target checkout: `/tmp/pbi-182-frozen-N3YU11` (no-hardlinks clone; clone source was the local origin checkout)
- Target HEAD before smoke: `8a1c9cac261a3b0ac8ec9c4ae503114ac608b98a`
- Target status before smoke: clean
- Target HEAD after smoke: `8a1c9cac261a3b0ac8ec9c4ae503114ac608b98a`
- Target status after smoke: clean
- pbi repository HEAD: `62f48a1a6bea7dee3637e2e833f8d959a2ce85f7`
- Command: `python3` subprocess with a `240s` timeout invoking `/usr/local/bin/pbi "Trace forced model alias request routing, ingress/model-detail rejection, model listing rewriting, response alias restoration, and hot reload generation atomicity. Give exact functions and key line ranges at current HEAD."`
- Return code: `1`
- Wall time: `19.694` seconds

## stdout

(empty)

## stderr

```text
pbi: partial source answer; verified candidates retained
Verified source evidence:
- crates/llm-guard-proxy-core/src/model_alias.rs:244 — resolver.resolve("missing"),
- crates/llm-guard-proxy-core/src/model_alias.rs:246 — model: String::from("missing"),
- crates/llm-guard-proxy-core/src/model_alias.rs:240 — fn rejects_unknown_alias() {
- crates/llm-guard-proxy-core/src/model_alias.rs:241 — let resolver = ModelAliasResolver::default();
- crates/llm-guard-proxy-core/src/model_alias.rs:243 — assert_eq!(
- crates/llm-guard-proxy-core/src/model_alias.rs:245 — Err(AliasResolutionError::UnknownAlias {
- crates/llm-guard-proxy-core/src/model_alias.rs:247 — })
Missing: requested target groups: ingress/model-detail rejection; response alias restoration; hot reload generation atomicity
```
