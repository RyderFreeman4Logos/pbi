#!/bin/sh
set -eu
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/pbi-rs"
fixture="$repo_root/tests/fixtures/repo"
probe="$repo_root/tests/fixtures/probe-fixture.sh"

verified=$(cd "$fixture" && PBI_RS_PROBE="$probe" "$binary" search "compression publication cache assembly")
printf '%s\n' "$verified" | grep -q '^Coverage: complete$'
printf '%s\n' "$verified" | grep -q '^Verified source evidence:$'
printf '%s\n' "$verified" | grep -q '^-[[:space:]]src/lib.rs:1 '
printf '%s\n' "$verified" | grep -q 'compression publication and cache key assembly'

raw=$(cd "$fixture" && PBI_RS_PROBE="$probe" "$binary" search --bm25 "compression publication cache assembly")
printf '%s\n' "$raw" | grep -q '^File: '

set +e
unrelated=$(cd "$fixture" && PBI_RS_PROBE="$probe" "$binary" search "ghost evidence" 2>/dev/null)
status=$?
set -e
test "$status" -eq 1
test -z "$unrelated"

debug=$($binary --debug-config)
printf '%s\n' "$debug" | grep -q '^api_key=\[REDACTED\]$'
printf '%s\n' "$debug" | grep -q '^search_default=compact_verified_bm25_no_chat$'
printf '%s\n' "$debug" | grep -q '^model_default_base_url=http://localhost:18317/v1$'
printf '%s\n' "$debug" | grep -q '^model_default_name=abliterated-qwen-latest-27b-none$'
printf '%s\n' "$debug" | grep -q '^model_credential_handles=CLIPROXY_API_KEY,OPENAI_API_KEY,LOCAL_ROUTER_API_KEY$'
printf '%s\n' "$debug" | grep -q '^model_binding=single_immutable_snapshot$'

real=$(cd "$repo_root" && "$binary" search "SourceLocation src/lib.rs")
printf '%s\n' "$real" | grep -q '^Coverage: complete$'
printf '%s\n' "$real" | grep -Eq '^-[[:space:]]src/lib.rs:[0-9]+(-[0-9]+)? '
printf '%s\n' 'acceptance: deterministic fixture and real Probe passed'
