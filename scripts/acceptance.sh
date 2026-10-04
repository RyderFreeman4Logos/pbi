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
canary=opaque-canary-acceptance-71c4
(cd "$fixture/repo" && pbi search "ghost_evidence $canary") > "$fixture/nohit.out" 2> "$fixture/nohit.err"
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
awk 'NR==1 && $0=="pbi: no source locations found" {ok=1} NR==2 && $0 ~ /^pbi-failure / && $0 ~ /\[REDACTED\]/ {receipt=1} END {exit !(ok && receipt && NR==2)}' "$fixture/nohit.err"
! grep -F "$canary" "$fixture/nohit.err"

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
printf '%s\n' "$debug" | grep -qx 'search_default=native_bounded_bm25_compact_no_probe'
printf '%s\n' "$debug" | grep -qx 'model_path=adk_workflow_kit_authorized_route_snapshot'
printf '%s\n' "$debug" | grep -qx 'api_key=\[REDACTED\]'
test ! -e "$marker"

# #312: the public CLI must share the existing BM25 formula and query evaluator.
python3 - "$binary" "$fixture" <<'PY'
import math
import pathlib
import subprocess
import sys

binary, fixture = sys.argv[1], pathlib.Path(sys.argv[2])
env = {"PATH": str(fixture / "bin") + ":/usr/bin:/bin", "HOME": str(fixture),
       "PBI_RS_ADK_ENABLE": "0", "PBI_PROBE_MARKER": str(fixture / "probe-invoked")}

def run(root, query, raw=False):
    return subprocess.run([binary, "search", *(["--bm25"] if raw else []), query],
                          cwd=root, env=env, capture_output=True, text=True, timeout=20)

rank = fixture / "rank"
rank.mkdir()
for name, text in [("high.txt", "foo foo foo foo"), ("one.txt", "foo"),
                   ("long.txt", "foo " + "noise " * 9)]:
    (rank / name).write_text(text + "\n")
expected = sorted([math.log(1 + 0.5 / 3.5) * tf * 2.2 /
                   (tf + 1.2 * (0.25 + 0.75 * length / 7))
                   for tf, length in [(4, 6), (1, 3), (1, 12)]], reverse=True)
scores = []
for raw in [False, True]:
    result = run(rank, "foo", raw)
    assert result.returncode == 0 and not result.stderr, result.stderr
    actual = [float(line[7:]) for line in result.stdout.splitlines() if line.startswith("Score: ")]
    assert len(actual) == 3
    assert all(abs(a - e) < 0.00005 for a, e in zip(actual, expected))
    scores.append(actual)
assert scores[0] == scores[1]
print("acceptance #312 row 1: three-file numeric Okapi scores/order passed")

boolean = fixture / "boolean"
boolean.mkdir()
for name, text in [("a.txt", "foo bar café 类型"), ("b.txt", "bar foo 类型 café"),
                   ("c.txt", "foo"), ("d.txt", "bar")]:
    (boolean / name).write_text(text + "\n")
for query, expected in [("foo OR bar", {"a.txt", "b.txt", "c.txt", "d.txt"}),
                        ("foo AND bar", {"a.txt", "b.txt"}), ("foo NOT bar", {"c.txt"}),
                        ('"foo bar"', {"a.txt"}), ('"café 类型"', {"a.txt"})]:
    for raw in [False, True]:
        result = run(boolean, query, raw)
        assert result.returncode == 0 and not result.stderr, result.stderr
        assert {name for name in ["a.txt", "b.txt", "c.txt", "d.txt"] if name in result.stdout} == expected
print("acceptance #312 rows 2-3: Boolean and ordered/Unicode phrases passed")
for raw in [False, True]:
    for query in ["foo OR", '"private_query_canary', "NOT foo"]:
        result = run(boolean, query, raw)
        assert result.returncode == 2 and not result.stdout
        assert "private_query_canary" not in result.stderr
print("acceptance #312 privacy: malformed syntax fails closed with static diagnostics")
PY

test ! -e "$marker"
printf '%s\n' 'acceptance: native bounded search, filters, root cap, no-hit, linked source, unified BM25, Boolean phrases, and Probe trap passed'
