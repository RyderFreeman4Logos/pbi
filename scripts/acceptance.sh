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
search_rc=$?
unrelated_error=$(cd "$fixture" && PBI_RS_PROBE="$probe" "$binary" search "ghost evidence" 2>&1 >/dev/null)
error_rc=$?
set -e
test "$search_rc" -eq 1
test "$error_rc" -eq 1
test -z "$unrelated"
test "$unrelated_error" = "pbi: no source locations found"

set +e
bare_miss=$(cd "$fixture" && PBI_RS_ADK_ENABLE=0 PBI_RS_PROBE="$probe" "$binary" "ghost evidence" 2>/dev/null)
bare_rc=$?
set -e
test "$bare_rc" -eq 1
test -z "$bare_miss"

bare_evidence=$(cd "$fixture" && PBI_RS_ADK_ENABLE=0 PBI_RS_PROBE="$probe" "$binary" "compression publication cache assembly")
printf '%s\n' "$bare_evidence" | grep -q '^Coverage: complete$'
printf '%s\n' "$bare_evidence" | grep -q 'compression publication and cache key assembly'

or_evidence=$(cd "$fixture" && PBI_RS_ADK_ENABLE=0 PBI_RS_PROBE="$probe" "$binary" "compression or cache")
printf '%s\n' "$or_evidence" | grep -q '^Coverage: complete$'
printf '%s\n' "$or_evidence" | grep -q '^-[[:space:]]src/lib.rs:1 '

debug=$($binary --debug-config)
printf '%s\n' "$debug" | grep -q '^api_key=\[REDACTED\]$'
printf '%s\n' "$debug" | grep -q '^search_default=compact_verified_bm25_no_chat$'
printf '%s\n' "$debug" | grep -q '^model_route_snapshot=ordered_authorized_candidates_bounded_by_kit$'
printf '%s\n' "$debug" | grep -q '^model_route_chain=repeatable_cli_routes_or_single_default$'
printf '%s\n' "$debug" | grep -q '^model_route_credentials=handle_names_only_values_not_emitted$'

real=$(cd "$repo_root/src" && "$binary" search "SourceLocation display_relative")
printf '%s\n' "$real" | grep -q '^Coverage: complete$'
printf '%s\n' "$real" | grep -Eq '^-[[:space:]]lib.rs:[0-9]+(-[0-9]+)? '
printf '%s\n' 'acceptance: deterministic fixture and real Probe passed'
