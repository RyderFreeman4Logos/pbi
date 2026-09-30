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
scoped=$(cd "$repo_root/src" && PBI_RS_ADK_ENABLE=0 "$binary" search "SourceLocation:display_relative")
printf '%s\n' "$scoped" | grep -q '^Coverage: complete$'
printf '%s\n' "$scoped" | grep -Eq '^-[[:space:]]lib.rs:[0-9]+(-[0-9]+)? '
printf '%s\n' "$scoped" | grep -q 'pub fn display_relative'
# Real Probe controls: each language selects its source; ignore can remove all hits.
filter_root=$(mktemp -d "${TMPDIR:?}/pbi-rs-filter-XXXXXX")
trap 'rm -rf -- "$filter_root"' EXIT HUP INT TERM
printf '%s\n' 'fn filter_operand_marker() {}' > "$filter_root/chosen.rs"
printf '%s\n' 'def filter_operand_marker(): pass' > "$filter_root/chosen.py"
mkdir "$filter_root/drafts"
printf '%s\n' 'fn filter_operand_marker() {}' > "$filter_root/drafts/hidden.rs"
for language in rust python; do
    filtered=$(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 --max-results=8 -l "$language" filter_operand_marker)
    printf '%s\n' "$filtered" | grep -q '^Coverage: complete$'
    case "$language" in rust) wanted=rs; unwanted=py ;; python) wanted=py; unwanted=rs ;; esac
    printf '%s\n' "$filtered" | grep -q "^- chosen.$wanted:1 "
    ! printf '%s\n' "$filtered" | grep -q "chosen.$unwanted"
    ! printf '%s\n' "$filtered" | grep -q 'drafts/'
done
ignored=$(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 -i '*.rs' filter_operand_marker)
printf '%s\n' "$ignored" | grep -q '^- chosen.py:1 '
! printf '%s\n' "$ignored" | grep -q 'chosen.rs'
set +e
no_hit=$(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 --ignore='*.rs' --ignore='*.py' filter_operand_marker 2> "$filter_root/no-hit.stderr")
no_hit_rc=$?
set -e
test "$no_hit_rc" -eq 1
test -z "$no_hit"
IFS= read -r no_hit_error < "$filter_root/no-hit.stderr"
test "$no_hit_error" = 'pbi: no source locations found'
raw_filtered=$(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --bm25 --timeout=3 -l rs -i '!drafts/**' filter_operand_marker)
printf '%s\n' "$raw_filtered" | grep -q 'File: .*chosen.rs'
! printf '%s\n' "$raw_filtered" | grep -q 'File: .*drafts/'
! printf '%s\n' "$raw_filtered" | grep -q 'File: .*chosen.py'
for flag in --help -h; do
    probe search "$flag" > "$filter_root/probe-help" 2> "$filter_root/probe-help.stderr"
    PBI_RS_ADK_ENABLE=0 "$binary" search "$flag" > "$filter_root/wrapper-help" 2> "$filter_root/wrapper-help.stderr"
    cmp "$filter_root/probe-help" "$filter_root/wrapper-help"
    cmp "$filter_root/probe-help.stderr" "$filter_root/wrapper-help.stderr"
done
reranked=$(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 -l rs --reranker=not-a-reranker filter_operand_marker)
printf '%s\n' "$reranked" | grep -q '^- chosen.rs:1 '
! printf '%s\n' "$reranked" | grep -q 'drafts/'
# Actual no-model question parsing must not let discarded routing values enter
# retrieval/coverage. Message mode keeps its single first question unchanged.
question_clean=$(cd "$repo_root/src" && PBI_RS_ADK_ENABLE=0 "$binary" "SourceLocation display_relative")
for mode in positional message; do
    case "$mode" in positional) set -- ;; message) set -- --message ;; esac
    question_sanitized=$(cd "$repo_root/src" && PBI_RS_ADK_ENABLE=0 "$binary" "$@" "SourceLocation display_relative" --model-name unapproved_model_route_canary --force-provider=remote_provider_canary)
    test "$question_sanitized" = "$question_clean"
done
# Native code-content budgets and post-ranking merging, not argv-only controls.
# Enough unrelated source avoids Probe's small-file/whole-file extraction path.
budget_root="$filter_root/budgets"
mkdir "$budget_root"
for index in $(seq 1 200); do
    printf 'fn unrelated_%s() -> u64 { %s }\n' "$index" "$index"
done > "$budget_root/sample.rs"
printf 'fn budget_marker_alpha() -> u64 {\n    let alpha = 1;\n    alpha + 10\n}\n' >> "$budget_root/sample.rs"
printf '\n\n\n\n\n\n\n\n\n\n\n\n' >> "$budget_root/sample.rs"
printf 'fn budget_marker_beta() -> u64 {\n    let beta = 2;\n    beta + 20\n}\n' >> "$budget_root/sample.rs"
for budget in --max-bytes=80 --max-tokens=30; do
    limited=$(cd "$budget_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --bm25 --timeout=3 "$budget" budget_marker)
    printf '%s\n' "$limited" | grep -q 'Found 1 search results'
    printf '%s\n' "$limited" | grep -q 'budget_marker_alpha'
    ! printf '%s\n' "$limited" | grep -q 'budget_marker_beta'
done
for threshold in 0 30; do
    merged=$(cd "$budget_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --bm25 --timeout=3 --merge-threshold "$threshold" budget_marker)
    case "$threshold" in 0) count=2 ;; 30) count=1 ;; esac
    printf '%s\n' "$merged" | grep -q "Found $count search results"
    printf '%s\n' "$merged" | grep -q 'budget_marker_alpha'
    printf '%s\n' "$merged" | grep -q 'budget_marker_beta'
done
for budget in --max-bytes=0 --max-tokens=0; do
    set +e
    budget_miss=$(cd "$budget_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 "$budget" budget_marker 2> "$filter_root/budget-miss.stderr")
    budget_rc=$?
    set -e
    test "$budget_rc" -eq 1
    test -z "$budget_miss"
    IFS= read -r budget_error < "$filter_root/budget-miss.stderr"
    test "$budget_error" = 'pbi: no source locations found'
done
budget_verified=$(cd "$budget_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 --max-results=1 --max-bytes=800 --max-tokens=300 --merge-threshold=30 budget_marker_alpha)
printf '%s\n' "$budget_verified" | grep -q '^Coverage: complete$'
printf '%s\n' "$budget_verified" | grep -q '^- sample.rs:201 '
# Native format ownership: no wrapper serializer; compare actual Probe bytes.
format_root="$filter_root/formats"
mkdir "$format_root"
printf '%s\n' 'fn format_operand_marker() {}' > "$format_root/chosen.rs"
for format in terminal markdown plain json xml color outline outline-xml; do
    (cd "$format_root" && probe search --timeout 3 --max-results 8 --reranker bm25 --language rs --ignore .git --ignore target --ignore drafts --ignore node_modules --ignore __pycache__ --format "$format" -- format_operand_marker) > "$filter_root/native-format" 2> "$filter_root/native-format.stderr"
    (cd "$format_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --bm25 --timeout=3 -l rs -o "$format" format_operand_marker) > "$filter_root/wrapper-format" 2> "$filter_root/wrapper-format.stderr"
    cmp "$filter_root/native-format" "$filter_root/wrapper-format"
    cmp "$filter_root/native-format.stderr" "$filter_root/wrapper-format.stderr"
    if test "$format" = json; then
        python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["summary"]["count"] == len(d["results"]) > 0; assert all("chosen.rs" in r["file"] for r in d["results"])' "$filter_root/wrapper-format"
    fi
done
format_verified=$(cd "$format_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --timeout=3 -l rs format_operand_marker)
printf '%s\n' "$format_verified" | grep -q '^Coverage: complete$'
printf '%s\n' "$format_verified" | grep -q '^- chosen.rs:1 '
printf '%s\n' "$format_verified" | grep -q 'fn format_operand_marker() {}'
# Legacy verified search appends plain; this Probe rejects duplicate formats.
set +e
(cd "$filter_root" && PBI_RS_ADK_ENABLE=0 "$binary" search --format=json filter_operand_marker) > "$filter_root/format-denied" 2> "$filter_root/format-denied.stderr"
format_rc=$?
set -e
test "$format_rc" -eq 2
test ! -s "$filter_root/format-denied"
grep -q -- '--format cannot be used multiple times' "$filter_root/format-denied.stderr"
printf '%s\n' 'acceptance: deterministic fixture and real Probe passed; native filters, question parsing, code budgets, merge thresholds and eight raw formats passed; verified format duplicate refused'
