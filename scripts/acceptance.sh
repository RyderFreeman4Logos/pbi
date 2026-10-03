#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/pbi-rs"
fixture=$(mktemp -d "${TMPDIR:?}/pbi-rs-acceptance-XXXXXX")
trap 'rm -rf -- "$fixture"' EXIT HUP INT TERM
mkdir "$fixture/bin" "$fixture/repo" "$fixture/capped"
marker="$fixture/probe-invoked"
for name in probe probe-chat; do
    cat > "$fixture/bin/$name" <<'TRAP'
#!/bin/sh
printf invoked > "$PBI_PROBE_MARKER"
exit 97
TRAP
    chmod +x "$fixture/bin/$name"
done

pbi() {
    env -i PATH="$fixture/bin:/usr/bin:/bin" HOME="$fixture" \
        TMPDIR="$TMPDIR" TMP="$TMPDIR" TEMP="$TMPDIR" \
        PBI_RS_ADK_ENABLE=0 PBI_RS_PROBE="$fixture/bin/probe" \
        PBI_PROBE_MARKER="$marker" "$binary" "$@"
}

printf '%s\n' 'fn filter_operand_marker() {}' > "$fixture/repo/chosen.rs"
printf '%s\n' 'def filter_operand_marker(): pass' > "$fixture/repo/chosen.py"
printf '%s\n' 'fn ignored_marker() {}' > "$fixture/repo/hidden.rs"
printf '%s\n' 'hidden.rs' > "$fixture/repo/.gitignore"

# The binary also works with a PATH that contains neither Probe executable.
plain=$(cd "$fixture/repo" && env -i PATH=/usr/bin:/bin HOME="$fixture" \
    TMPDIR="$TMPDIR" PBI_RS_ADK_ENABLE=0 "$binary" search filter_operand_marker)
printf '%s\n' "$plain" | grep -qx 'chosen.rs:1'
printf '%s\n' "$plain" | grep -qx 'chosen.py:1'

rust=$(cd "$fixture/repo" && pbi search -l rs filter_operand_marker)
printf '%s\n' "$rust" | grep -qx 'chosen.rs:1'
! printf '%s\n' "$rust" | grep -q 'chosen.py'
python=$(cd "$fixture/repo" && pbi search -l python filter_operand_marker)
printf '%s\n' "$python" | grep -qx 'chosen.py:1'
! printf '%s\n' "$python" | grep -q 'chosen.rs'
ignored=$(cd "$fixture/repo" && pbi search -i '*.rs' filter_operand_marker)
printf '%s\n' "$ignored" | grep -qx 'chosen.py:1'
! printf '%s\n' "$ignored" | grep -q 'chosen.rs'

set +e
(cd "$fixture/repo" && pbi search ignored_marker) > "$fixture/ignored.out" 2> "$fixture/ignored.err"
ignored_rc=$?
(cd "$fixture/repo" && pbi search ghost_evidence) > "$fixture/nohit.out" 2> "$fixture/nohit.err"
nohit_rc=$?
(cd "$fixture/repo" && pbi search --bm25 filter_operand_marker) > "$fixture/raw.out" 2> "$fixture/raw.err"
raw_rc=$?
set -e
test "$ignored_rc" -eq 1
test "$nohit_rc" -eq 1
test "$raw_rc" -eq 0
test ! -s "$fixture/ignored.out"
test ! -s "$fixture/nohit.out"
grep -q '^File: chosen.rs, Lines: 1-1$' "$fixture/raw.out"
grep -q '^File: chosen.py, Lines: 1-1$' "$fixture/raw.out"
test ! -s "$fixture/raw.err"
grep -qx 'pbi: no source locations found' "$fixture/nohit.err"

printf '%s\n' 'fn outside_marker() {}' > "$fixture/outside.rs"
ln -s "$fixture/outside.rs" "$fixture/repo/linked.rs"
set +e
(cd "$fixture/repo" && pbi search outside_marker) > "$fixture/linked.out" 2> "$fixture/linked.err"
linked_rc=$?
(cd "$fixture/repo" && pbi search --bm25 outside_marker) > "$fixture/raw-linked.out" 2> "$fixture/raw-linked.err"
raw_linked_rc=$?
set -e
test "$linked_rc" -eq 1
test ! -s "$fixture/linked.out"
test "$raw_linked_rc" -eq 1
test ! -s "$fixture/raw-linked.out"

for index in $(seq 1 17); do
    printf '%s\n' 'fn capped_marker() {}' > "$fixture/capped/file-$index.rs"
done
set +e
(cd "$fixture/capped" && pbi search capped_marker) > "$fixture/capped.out" 2> "$fixture/capped.err"
capped_rc=$?
(cd "$fixture/capped" && pbi search --bm25 capped_marker) > "$fixture/raw-capped.out" 2> "$fixture/raw-capped.err"
raw_capped_rc=$?
set -e
test "$capped_rc" -eq 1
grep -q 'bounded target limit' "$fixture/capped.err"
test "$raw_capped_rc" -eq 1
test ! -s "$fixture/raw-capped.out"
grep -q 'bounded target limit' "$fixture/raw-capped.err"

debug=$(pbi --debug-config)
printf '%s\n' "$debug" | grep -qx 'search_default=native_bounded_term_frequency_no_probe'
printf '%s\n' "$debug" | grep -qx 'model_path=adk_workflow_kit_authorized_route_snapshot'
printf '%s\n' "$debug" | grep -qx 'api_key=\[REDACTED\]'
test ! -e "$marker"
printf '%s\n' 'acceptance: native bounded search, filters, root cap, no-hit, linked source, raw BM25, and Probe trap passed'
