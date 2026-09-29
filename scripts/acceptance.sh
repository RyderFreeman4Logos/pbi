#!/bin/sh
set -eu
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/pbi-rs"
fixture="$repo_root/tests/fixtures/repo"
probe="$repo_root/tests/fixtures/probe-fixture.sh"

verified=$(cd "$fixture" && PBI_RS_PROBE="$probe" "$binary" search "compression publication cache assembly")
test "$verified" = 'src/lib.rs:1'

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

real=$(cd "$repo_root" && "$binary" search "verify probe locations")
printf '%s\n' "$real" | grep -Eq '^src/lib.rs:[0-9]+(-[0-9]+)?$'
printf '%s\n' 'acceptance: deterministic fixture and real Probe passed'
