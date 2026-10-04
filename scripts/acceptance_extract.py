#!/usr/bin/env python3
"""Issue #314 public-CLI acceptance; works with debug or staged release bytes."""
import hashlib
import json
import os
import pathlib
import subprocess
import sys

binary, fixture, repo = map(pathlib.Path, sys.argv[1:])
binary = binary.absolute()
root = fixture / "extract"
root.mkdir()
source = "// outside\n\n/// café documentation\n#[inline]\npub fn selected() {\n    let text = \"é }\";\n    fn nested() {\n        let _ = text_marker();\n    }\n    nested();\n}\nfn next() {}\n"
(root / "fixture.rs").write_text(source)
# Provider egress cannot be reached by extract, even with model use enabled.
env = {"PATH": str(fixture / "bin") + ":/usr/bin:/bin", "HOME": str(fixture),
       "PBI_RS_ADK_ENABLE": "1", "PBI_RS_MODEL_BASE_URL": "http://127.0.0.1:1/v1",
       "PBI_PROBE_MARKER": str(fixture / "probe-invoked")}
calls = []

def run(position, *options, cwd=root):
    argv = [str(binary), "extract", position, *options]
    result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=20)
    calls.append({"argv": argv, "cwd": str(cwd), "rc": result.returncode,
                  "stdout_bytes": len(result.stdout), "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
                  "stderr_sha256": hashlib.sha256(result.stderr).hexdigest()})
    return result

def expected(path, start, end, body, status="complete"):
    return f"File: {path}, Lines: {start}-{end}\nBlock: {status}\n\n{body}\n".encode()

result = run("fixture.rs:6")
assert result.returncode == 0 and not result.stderr
assert result.stdout == expected("fixture.rs", 3, 11, "\n".join(source.splitlines()[2:11]))
assert b"fn next" not in result.stdout
nested = run("fixture.rs:8")
assert nested.returncode == 0 and nested.stdout == expected("fixture.rs", 7, 9, "\n".join(source.splitlines()[6:9]))
print("acceptance #314 row 1: complete attributed UTF-8 function and nested boundaries rc=0")

result = run("fixture.rs:1")
assert result.returncode == 0 and result.stdout == expected("fixture.rs", 1, 4, "\n".join(source.splitlines()[:4]), "approximate")
print("acceptance #314 row 2: outside item bounded approximate window rc=0")

(root / "huge.rs").write_text('fn huge() {\n    let _ = "' + "é" * 30000 + '";\n}\n')
for cap in [32768, 128, 100000]:
    result = run("huge.rs:2", "--max-bytes", str(cap))
    assert result.returncode == 0 and len(result.stdout) <= min(cap, 32768)
    assert "Block: truncated" in result.stdout.decode() and result.stdout.endswith(b"\n[truncated]\n")
print("acceptance #314 row 3: byte caps and explicit UTF-8-safe truncation rc=0")

(root / ".gitignore").write_text("ignored.rs\n!.env\n")
(root / "ignored.rs").write_text("fixture_private_canary")
(root / ".env").write_text("fixture_private_canary")
(root / "link.rs").symlink_to("fixture.rs")
(root / "proc").symlink_to("/proc")
(root / "fifo.rs").touch()
(root / "fifo.rs").unlink()
os.mkfifo(root / "fifo.rs")
(root / "oversize.rs").write_bytes(b"a" * (2 * 1024 * 1024 + 1))
for position in ["../outside.rs:1", "/proc/version:1", "proc/version:1", "link.rs:1", ".env:1", "ignored.rs:1", "fifo.rs:1", "oversize.rs:1", "fixture.rs:0", "fixture.rs:999"]:
    result = run(position)
    assert result.returncode != 0 and not result.stdout
    assert position.encode() not in result.stderr and b"fixture_private_canary" not in result.stderr
result = run("fixture.rs:6", "--timeout", "0")
assert result.returncode == 1 and not result.stdout and b"deadline_s=0" in result.stderr
print("acceptance #314 row 4/bounds: containment, ignores, no-follow, nonfiles, size, positions, deadline, static privacy errors passed")

# Known executable source, not a snippet dump or injected final span.
path = repo / "src/native_search.rs"
real_source = path.read_text()
start_offset = real_source.index("fn raw_terms(query: &str)")
end_offset = real_source.index("\n}\n", start_offset) + 2
start_line = real_source.count("\n", 0, start_offset) + 1
end_line = real_source.count("\n", 0, end_offset) + 1
result = run(f"src/native_search.rs:{start_line + 4}", cwd=repo)
assert result.returncode == 0 and not result.stderr
assert result.stdout == expected("src/native_search.rs", start_line, end_line, real_source[start_offset:end_offset])
assert b"fn read_source" not in result.stdout
print(f"acceptance #314 known source: src/native_search.rs:{start_line}-{end_line} exact complete bytes rc=0")
assert not (fixture / "probe-invoked").exists()
print("acceptance #314 no model/Probe: enabled-model decoy route and Probe traps untouched")
print(json.dumps({"issue": 314, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "calls": calls, "call_count": len(calls), "known_source_sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "known_span": [start_line, end_line], "rows": [1, 2, 3, 4], "row_5": "separate baseline RED receipt required"}, sort_keys=True))
