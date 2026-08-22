#!/usr/bin/env python3
"""Focused hermetic checks for the pbi Probe Chat wrapper."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent
PBI = ROOT / "pbi"
PRIMARY = "qwen3.6-27b-decensor-by-aeon"
FALLBACK = "opencode/deepseek-v4-flash"
BASE_URL = "http://localhost:8317/v1"
PROBE_SHIM = "/usr/local/share/mise/shims/probe"


class PbiTest(unittest.TestCase):
    def run_pbi(
        self,
        *args: str,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
        binary: Path = PBI,
        timeout: float | None = None,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(binary), *args],
            text=True,
            capture_output=True,
            check=False,
            env=env,
            cwd=cwd,
            timeout=timeout,
        )

    def fake_environment(self, directory: Path) -> tuple[dict[str, str], Path]:
        trace = directory / "trace.json"
        fake_probe = directory / "probe"
        fake_probe.write_text("#!/usr/bin/env bash\nexit 0\n")
        fake_probe.chmod(0o755)
        (directory / "mise").write_text(
            "#!/usr/bin/env bash\n"
            "set -eu\n"
            "[ \"$1\" = which ] && [ \"$2\" = probe ]\n"
            "printf '%s\\n' \"$PBI_TEST_PROBE\"\n"
        )
        (directory / "probe-chat").write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "keys = ('PROBE_BINARY_PATH', 'FORCE_PROVIDER', 'MODEL_NAME', 'OPENAI_API_KEY', "
            "'OPENAI_API_URL', 'LLM_BASE_URL', 'REQUEST_TIMEOUT', "
            "'MAX_OPERATION_TIMEOUT', 'MAX_RETRIES', 'FALLBACK_PROVIDERS', 'ALLOWED_FOLDERS')\n"
            "with open(os.environ['PBI_TEST_TRACE'], 'w') as f:\n"
            "    json.dump({'argv': sys.argv[1:], 'env': {k: os.environ.get(k) for k in keys}}, f)\n"
            "raise SystemExit(23)\n"
        )
        (directory / "npx").write_text("#!/usr/bin/env bash\nexit 24\n")
        for command in (directory / "mise", directory / "probe-chat", directory / "npx"):
            command.chmod(0o755)
        env = os.environ.copy()
        for name in ("LOCAL_MODEL", "LLM_MODEL", "FALLBACK_MODEL", "XDG_CONFIG_HOME", "PBI_CONFIG_FILE"):
            env.pop(name, None)
        env["PATH"] = f"{directory}:{env['PATH']}"
        env["PBI_TEST_TRACE"] = str(trace)
        env["PBI_TEST_PROBE_TRACE"] = str(directory / "probe-trace.json")
        env["PBI_TEST_PROBE"] = str(fake_probe)
        env["HOME"] = str(directory)
        env["CLIPROXY_API_KEY"] = "test-key"
        env["MAX_RETRIES"] = "1"
        return env, trace

    def fake_pbi(self, directory: Path, fake_probe: Path) -> Path:
        binary = directory / "pbi"
        binary.write_text(PBI.read_text().replace(PROBE_SHIM, str(fake_probe)))
        binary.chmod(0o755)
        return binary

    def record_probe_argv(self, probe: Path) -> None:
        probe.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
            "    json.dump(sys.argv[1:], f)\n"
        )
        probe.chmod(0o755)

    def record_probe_invocation(self, probe: Path) -> None:
        probe.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "keys = ('FORCE_PROVIDER', 'MODEL_NAME', 'OPENAI_API_KEY', "
            "'OPENAI_API_URL', 'MAX_RETRIES', 'FALLBACK_PROVIDERS')\n"
            "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
            "    json.dump({'argv': sys.argv[1:], 'env': {k: os.environ.get(k) for k in keys}}, f)\n"
        )
        probe.chmod(0o755)

    def test_static_interface_never_starts_an_agent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env, trace = self.fake_environment(Path(temporary))
            help_result = self.run_pbi("--help", env=env)
            version_result = self.run_pbi("--version", env=env)
            no_args_result = self.run_pbi(env=env)
            self.assertFalse(trace.exists(), "static commands must not launch Probe Chat")
        self.assertEqual(help_result.returncode, 0, help_result.stderr)
        self.assertIn("Probe Chat wrapper", help_result.stdout)
        self.assertEqual(version_result.returncode, 0, version_result.stderr)
        self.assertIn("pbi", version_result.stdout)
        self.assertIn("Usage: pbi <question...>", help_result.stdout)
        self.assertIn("pbi search [--bm25] <query>", help_result.stdout)
        self.assertEqual(no_args_result.returncode, 2)
        self.assertIn("question is required", no_args_result.stderr)

    def test_bm25_search_skips_an_unconfigured_mise_probe_shim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            shim_dir = directory / "shim-root" / "mise" / "shims"
            real_bin = directory / "real-bin"
            shim_dir.mkdir(parents=True)
            real_bin.mkdir()
            broken_probe = shim_dir / "probe"
            broken_probe.write_text(
                "#!/usr/bin/env bash\nprintf '%s\n' 'mise ERROR No version is set for shim: probe' >&2\nexit 1\n"
            )
            broken_probe.chmod(0o755)
            real_probe = real_bin / "probe"
            real_probe.write_text("#!/usr/bin/env bash\nprintf '%s\n' 'real.py:1'\n")
            real_probe.chmod(0o755)
            env["PATH"] = f"{shim_dir}:{real_bin}:{directory}:/usr/bin:/bin"
            env["PBI_TEST_PROBE"] = str(directory / "missing-probe")
            result = self.run_pbi(
                "search", "--bm25", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, broken_probe)
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertNotIn("mise ERROR", result.stdout + result.stderr)

    def test_local_routing_skips_an_unconfigured_mise_node_shim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            shim_dir = directory / "shim-root" / "mise" / "shims"
            real_bin = directory / "real-bin"
            shim_dir.mkdir(parents=True)
            real_bin.mkdir()
            broken_node = shim_dir / "node"
            broken_node.write_text(
                "#!/usr/bin/env bash\nprintf '%s\n' 'mise ERROR No version is set for shim: node' >&2\nexit 1\n"
            )
            broken_node.chmod(0o755)
            real_node = real_bin / "node"
            real_node.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if 'PBI_BASE_URL' in sys.argv[-1]:\n"
                "    print('[]')\n"
                "else:\n"
                "    raise SystemExit(1)\n"
            )
            real_node.chmod(0o755)
            env["PATH"] = f"{shim_dir}:{real_bin}:{directory}:/usr/bin:/bin"
            result = self.run_pbi("--message", "hello", env=env, binary=self.fake_pbi(directory, directory / "probe"))
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe-chat failed\n")
        self.assertNotIn("mise ERROR", result.stdout + result.stderr)

    def test_positional_question_is_compact_chat_and_preserves_json_request(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            (directory / "main.rs").write_text("struct Cli {}\nfn main() { Cli::parse(); }\n")
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as f:\n"
                "    print(json.dumps(sys.argv[1:]), file=f)\n"
                "if \"--dry-run\" in sys.argv:\n"
                "    raise SystemExit(0)\n"
                f"print('File: {directory / 'pbi'}, Lines: 1-40')\n"
                "print('readonly PBI_VERSION=0.1.0')\n"
                "print('query=' + sys.argv[-1])\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'a') as f:\n"
                "    print(json.dumps({'argv': sys.argv[1:]}), file=f)\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('entrypoint CLI parsing')\n"
                "    print('command dispatch match')\n"
                "    print('clap Subcommand derive')\n"
                "    print('persistence write callers')\n"
                "    print('result return formatting')\n"
                "elif message.startswith('Review and compress the draft answer'):\n"
                "    print('The entrypoint is pbi:1.')\n"
                "elif message.startswith('Audit every source citation'):\n"
                "    print('The entrypoint is pbi:1.')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    if 'refinement round 2 of 2' in message:\n"
                "        print('NONE')\n"
                "    else:\n"
                "        print('readonly PBI_VERSION')\n"
                "        print('compact_search_locations')\n"
                "        print('DEFAULT_SEARCH_TIMEOUT_SECONDS')\n"
                "else:\n"
                "    print(f'- {os.getcwd()} ✓')\n"
                "    print('The entrypoint is pbi:1.')\n"
                "    print('AI SDK Warning: System messages are risky.', file=sys.stderr)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where",
                "is",
                "the",
                "entrypoint",
                "--json",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
            recorded = [json.loads(line) for line in trace.read_text().splitlines()]
            probe_trace = directory / "probe-trace.json"
            self.assertTrue(probe_trace.exists(), "positional questions must retrieve code first")
            probe_calls = [json.loads(line) for line in probe_trace.read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "The entrypoint is pbi:1.\n")
        self.assertEqual(
            [call[-1] for call in probe_calls],
            [
                "entrypoint",
                "where is the entrypoint",
                "entrypoint CLI parsing",
                "command dispatch match",
                "clap Subcommand derive",
                "persistence write callers",
                "result return formatting",
                "readonly PBI_VERSION",
                "compact_search_locations",
                "DEFAULT_SEARCH_TIMEOUT_SECONDS",
            ],
        )
        self.assertTrue(all(call[1:7] == ["--timeout", "540", "--max-results", "4", "--max-tokens", "4000"] for call in probe_calls))
        self.assertEqual(len(recorded), 6)
        planner, gap_planner, second_gap_planner, draft, reviewer, citation_auditor = recorded
        self.assertIn("Convert the code question", planner["argv"][5])
        self.assertNotIn("Repository files:\n", planner["argv"][5])
        self.assertEqual(planner["argv"][6:8], ["--max-iterations", "1"])
        self.assertIn("Identify missing evidence", gap_planner["argv"][5])
        self.assertNotIn("Repository files:\n", gap_planner["argv"][5])
        self.assertIn("query=entrypoint CLI parsing", gap_planner["argv"][5])
        self.assertIn("Identify missing evidence", second_gap_planner["argv"][5])
        self.assertIn("query=readonly PBI_VERSION", second_gap_planner["argv"][5])
        self.assertEqual(draft["argv"][:5], ["--force-provider", "openai", "--model-name", PRIMARY, "--message"])
        self.assertIn("Question: where is the entrypoint", draft["argv"][5])
        self.assertIn("Code excerpts:\nFile:", draft["argv"][5])
        self.assertIn("query=DEFAULT_SEARCH_TIMEOUT_SECONDS", draft["argv"][5])
        self.assertIn("Exact literal matches for readonly PBI_VERSION", draft["argv"][5])
        self.assertIn("Repository entrypoint landmarks", draft["argv"][5])
        self.assertIn("main.rs:2", draft["argv"][5])
        self.assertEqual(draft["argv"][6:8], ["--max-iterations", "1"])
        self.assertTrue(all("--prompt" not in call["argv"] for call in recorded))
        self.assertIn("Review and compress the draft answer", reviewer["argv"][5])
        self.assertIn("Draft answer:\nThe entrypoint is pbi:1.", reviewer["argv"][5])
        self.assertIn("Evidence:\nFile:", reviewer["argv"][5])
        self.assertEqual(reviewer["argv"][-1], "--json")
        self.assertIn("Audit every source citation", citation_auditor["argv"][5])
        self.assertIn("Answer to audit:\nThe entrypoint is pbi:1.", citation_auditor["argv"][5])
        self.assertIn("Source evidence:\nFile:", citation_auditor["argv"][5])
        self.assertEqual(citation_auditor["argv"][-1], "--json")

    def test_default_query_chat_signal_emits_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            tmpdir = directory / "tmp"
            tmpdir.mkdir()
            env["TMPDIR"] = str(tmpdir)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$*\" == *--dry-run* ]]; then\n"
                "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-1'\n"
                "else\n"
                + "    printf '%s\\n' 'File: "
                + str(directory / "pbi")
                + ", Lines: 1-1'\n"
                "fi\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import signal, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('entrypoint')\n"
                "    print('main')\n"
                "    print('dispatch')\n"
                "    print('handler')\n"
                "    print('test')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "    while True: time.sleep(0.1)\n"
            )
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = subprocess.run(
                [
                    "timeout",
                    "--kill-after=1s",
                    "2s",
                    str(self.fake_pbi(directory, probe)),
                    "where is the entrypoint",
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                cwd=ROOT,
                timeout=6,
            )
            elapsed = time.monotonic() - started
            self.assertEqual(list(tmpdir.iterdir()), [])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe-chat timed out answering the question\n")
        self.assertLess(elapsed, 5)

    def test_default_query_ambient_duplicate_stamps_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'candidate.py:1'\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    for query in ('query one', 'query two', 'query three', 'query four', 'query five'):\n"
                "        print(query)\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('candidate.py:1')\n"
                "    print('hermes:ambient')\n"
                "    print('candidate.py:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "404?",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

    def test_default_query_bm25_fast_path_requires_distinctive_token_on_cited_line(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            generic = source_dir / "global_tests.rs"
            target = source_dir / "worktree_reclaim_tests.rs"
            generic.write_text("fn lookup() {}\nsession: Default::default()\n")
            target.write_text("// filler\n" * 44 + "fn worktree_write_lock_reclaims_terminal_session_after_holder_crash() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ[\"PBI_TEST_PROBE_TRACE\"], \"a\") as trace: print(json.dumps(query), file=trace)\n"
                f"if query == \"lock_reclaim\": print(\"{target}:45\")\n"
                "else: print(\"git-fixtures:1\")\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where are late-alias lock-reclaim?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [
                json.loads(line)
                for line in (directory / "probe-trace.json").read_text().splitlines()
            ]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/worktree_reclaim_tests.rs:45\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("lock_reclaim", probe_queries)
        self.assertFalse(any("late_alias lock_reclaim" in query for query in probe_queries))

    def test_default_query_bm25_fast_path_ignores_generic_findings_default_line(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            generic = source_dir / "review_cmd_output_consistency_helpers.rs"
            target = source_dir / "repo_write_audit.rs"
            generic.write_text("write_findings_toml(session_dir, &FindingsFile::default())\n")
            target.write_text("\n" * 124 + "fn append_repo_write_audit_finding() { let _ = FINDINGS_TOML_SYNTHETIC_MARKER; }\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ[\"PBI_TEST_PROBE_TRACE\"], \"a\") as trace: print(json.dumps(query), file=trace)\n"
                f"if query == \"appending\": print(\"{target}:125\")\n"
                "else: print(\"git-fixtures:1\")\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where is appending review audit",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [
                json.loads(line)
                for line in (directory / "probe-trace.json").read_text().splitlines()
            ]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/repo_write_audit.rs:125\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("appending", probe_queries)
        self.assertNotIn("append", probe_queries)

    def test_default_query_bm25_fast_path_skips_slow_planner(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            ts_source = repo / "src" / "ipc.ts"
            py_source = repo / "src" / "codex.py"
            ts_source.parent.mkdir(parents=True)
            ts_source.write_text("// filler\n" * 66 + "const key = `${desktopFsCacheKey()}:x`\n" + "// filler\n" * 10)
            py_source.write_text("def _bounded_prompt_cache_key(): pass\n" "def _content_cache_key(): pass\n")
            env, _ = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace:\n"
                "    trace.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "print('File: src/ipc.ts, Lines: 66-76')\n"
                "print('Remaining files not shown:')\n"
                "print(' src/codex.py <3> <46>')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import signal, time\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "while True: time.sleep(0.1)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is compression publication and main route cache key assembly for first post-compress request",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=4,
            )
            probe_trace = directory / "probe-trace.json"
            self.assertTrue(probe_trace.exists(), "BM25 fast path must search before planner")
            probe_calls = [json.loads(line) for line in probe_trace.read_text().splitlines()]
            self.assertFalse((directory / "trace.json").exists(), "a fast-path success must not start the planner")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/codex.py:2\n")
        self.assertEqual(result.stderr, "")
        self.assertTrue(probe_calls)
        fast_query = probe_calls[0][-1]
        self.assertEqual(fast_query, "cache_key")
        self.assertNotIn("post_compress", fast_query)
        self.assertNotIn("compress", fast_query)
        self.assertNotIn("cache key", fast_query)

    def test_default_query_without_distinctive_tokens_runs_bm25_without_planner(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "with open(os.environ[\"PBI_TEST_PROBE_TRACE\"], \"w\") as trace: trace.write(\"searched\")\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where is the session wait helper",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
            probe_query = (directory / "probe-trace.json").read_text()
            planner_started = (directory / "trace.json").exists()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertEqual(probe_query, "searched")
        self.assertFalse(planner_started)

    def test_default_query_bm25_fast_path_timeout_fails_closed_without_planner(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nsleep 30\n")
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where is compression publication and cache key assembly?",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
                timeout=12,
            )
            elapsed = time.monotonic() - started
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertFalse(trace.exists(), "a timed-out fast path must not start planner or chat")
        self.assertLess(elapsed, 10)

    def test_default_query_bm25_fast_path_skips_unrelated_first_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            unrelated = source_dir / "unrelated.py"
            target = source_dir / "target.py"
            unrelated.write_text("# unrelated candidate\n" * 5)
            target.write_text("# target\n" * 6 + "def _content_cache_key(): pass\n")
            env, _ = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "with open(os.environ[\"PBI_TEST_PROBE_TRACE\"], \"a\") as trace: trace.write(sys.argv[-1] + \"\\n\")\n"
                f"print(\"{unrelated}:5\")\n"
                f"print(\"{target}:7\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import signal, time\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "while True: time.sleep(0.1)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is compression publication and cache key assembly audit?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=4,
            )
            probe_trace = directory / "probe-trace.json"
            self.assertTrue(probe_trace.exists(), "BM25 fast path must search before planner")
            self.assertFalse((directory / "trace.json").exists(), "a token-matched fast path must not start the planner")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/target.py:7\n")
        self.assertEqual(result.stderr, "")

    def test_default_query_bm25_fast_path_ignores_generic_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            first = source_dir / "markers.rs"
            target = source_dir / "target.rs"
            first.write_text("// filler\n" * 4 + "let markers = scan_markers(&lines);\n")
            target.write_text("// filler\n" * 4 + "let provenance = compression;\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"{first}:5\")\n"
                f"print(\"{target}:5\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import os, signal, time\n"
                "with open(os.environ[\"PBI_TEST_TRACE\"], \"w\") as trace: trace.write(\"planner\")\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "while True: time.sleep(0.1)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the marker oauth provenance compression path?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=4,
            )
            planner_trace = directory / "trace.json"
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/target.rs:5\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(planner_trace.exists(), "a token-matched fast path must not start the planner")

    def test_default_query_bm25_fast_path_rejects_unrelated_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            unrelated = source_dir / "unrelated.py"
            unrelated.write_text("# unrelated candidate\n" * 7)
            env, _ = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"{unrelated}:5\")\n"
                f"print(\"{unrelated}:7\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import os, signal, time\n"
                "with open(os.environ[\"PBI_TEST_TRACE\"], \"w\") as trace: trace.write(\"planner\")\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "while True: time.sleep(0.1)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is compression publication and cache key assembly?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=4,
            )
            planner_trace = directory / "trace.json"
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertFalse(planner_trace.exists(), "a BM25 miss must not start the planner")
        self.assertNotIn("unrelated.py:", result.stdout + result.stderr)

    def test_default_query_timeout_recovers_named_symbol_definitions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src" / "api"
            source_dir.mkdir(parents=True)
            (source_dir / "mcp.rs").write_text(
                "// reserve_http_session is documented here, not defined\n"
                "fn reap_expired_sessions() {}\n"
                "fn reserve_http_session() {}\n"
            )
            (source_dir / "mcp_reservation_tests.rs").write_text(
                "fn reservation_tests_cover_mcp() {}\n"
            )
            env, _ = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
                "    f.write('invoked')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import signal, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "    while True: time.sleep(0.1)\n"
                "raise SystemExit(2)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are reserve_http_session, reap_expired_sessions, and their MCP reservation tests, and what is the current lookup flow?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=4,
            )
            probe_trace = directory / "probe-trace.json"
            self.assertTrue(probe_trace.exists(), "BM25 fast path must run before planner")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp.rs:2\nsrc/api/mcp.rs:3\n")
        self.assertEqual(result.stderr, "")

    def test_planner_warning_mixed_bm25_stamps_recover_named_symbol_definitions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_file = repo / "src" / "api" / "mcp.rs"
            source_file.parent.mkdir(parents=True)
            source_file.write_text(
                "fn reap_expired_sessions() {}\n"
                "fn reserve_http_session() {}\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if \"--dry-run\" not in sys.argv:\n"
                f"    print('File: {source_file}, Lines: 1-20')\n"
                "    print('1970-01-01T00:00')\n"
                "    print('127.0.0.1:3080')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'a') as f:\n"
                "    f.write(message.splitlines()[0] + '\\n')\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('AI SDK Warning: System messages are risky.', file=sys.stderr)\n"
                "    raise SystemExit(2)\n"
                "raise SystemExit(99)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are reserve_http_session and reap_expired_sessions?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            chat_calls = trace.read_text().splitlines()
        self.assertEqual(chat_calls, ["Convert the code question into exactly five complementary Probe BM25 code-search queries. Cover the user's terminology, likely identifiers, entry points and callers, data or control flow, and tests or configuration. Return exactly five plain lines, with no bullets, quotes, or explanation: where are reserve_http_session and reap_expired_sessions?"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp.rs:1\nsrc/api/mcp.rs:2\n")
        self.assertEqual(result.stderr, "")
        self.assertNotIn("1970-01-01T00:00", result.stdout)
        self.assertNotIn("127.0.0.1:3080", result.stdout)

    def test_default_query_mixed_stamps_recover_named_symbol_definitions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_file = repo / "src" / "api" / "mcp.rs"
            source_file.parent.mkdir(parents=True)
            source_file.write_text(
                "fn reap_expired_sessions() {}\n"
                "fn reserve_http_session() {}\n"
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if \"--dry-run\" not in sys.argv:\n"
                f"    print('File: {source_file}, Lines: 1-20')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('dummy query one')\n"
                "    print('dummy query two')\n"
                "    print('dummy query three')\n"
                "    print('dummy query four')\n"
                "    print('dummy query five')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('src/core/db.rs:1')\n"
                "    print('1970-01-01T00:00')\n"
                "    print('src/mcp/daemon_rest.rs:1')\n"
                "    print('127.0.0.1:3080')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are reserve_http_session and reap_expired_sessions?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp.rs:1\nsrc/api/mcp.rs:2\n")
        self.assertEqual(result.stderr, "")
        self.assertNotIn("1970-01-01T00:00", result.stdout)
        self.assertNotIn("127.0.0.1:3080", result.stdout)

    def test_default_query_mixed_stamp_with_cited_symbol_recovers_or_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "pbi"
            source.write_text("# source header\nconst PBI_VERSION = 'test'\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('PBI_VERSION lookup')\n"
                "    print('PBI_VERSION definition')\n"
                "    print('pbi entrypoint')\n"
                "    print('pbi configuration')\n"
                "    print('pbi tests')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('pbi:1')\n"
                "    print('1970-01-01T00:00')\n"
                "    print('127.0.0.1:3080')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is PBI_VERSION?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:2\n")
        self.assertNotIn("1970-01-01T00:00", result.stdout)
        self.assertNotIn("127.0.0.1:3080", result.stdout)

    def test_default_query_without_identifier_tokens_does_not_silent_exit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-40'\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('one')\n"
                "    print('two')\n"
                "    print('three')\n"
                "    print('four')\n"
                "    print('five')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('pbi:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "404?",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

    def test_default_query_identifier_free_dotted_file_citation_succeeds(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('dummy query one')\n"
                "    print('dummy query two')\n"
                "    print('dummy query three')\n"
                "    print('dummy query four')\n"
                "    print('dummy query five')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('main.rs:42')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "404?",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "main.rs:42\n")
        self.assertEqual(result.stderr, "")

    def test_default_query_identifier_free_mixed_stamps_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('dummy query one')\n"
                "    print('dummy query two')\n"
                "    print('dummy query three')\n"
                "    print('dummy query four')\n"
                "    print('dummy query five')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('pbi:1')\n"
                "    print('1970-01-01T00:00')\n"
                "    print('127.0.0.1:3080')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "404?",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertNotIn("1970-01-01T00:00", result.stdout)
        self.assertNotIn("127.0.0.1:3080", result.stdout)

    def test_default_query_mixed_stamp_recovery_does_not_claim_absence_without_rg(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "pbi"
            source.write_text("# source header\nconst PBI_VERSION = 'test'\n")
            env, _ = self.fake_environment(directory)
            tool_path = directory / "tool-path"
            tool_path.mkdir()
            for name in (
                "bash", "python3", "env", "sleep", "timeout", "setsid", "sh", "mktemp", "rm", "realpath",
                "grep", "awk", "sort", "cut", "sed", "head", "readlink",
            ):
                command = shutil.which(name)
                if command:
                    (tool_path / name).symlink_to(command)
            env["PATH"] = os.pathsep.join((str(directory), str(tool_path)))
            node = directory / "node"
            node.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "if os.environ.get('PBI_BASE_URL'):\n"
                "    print('[]')\n"
                "else:\n"
                "    sys.stdin.read()\n"
                "    raise SystemExit(1)\n"
            )
            node.chmod(0o755)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$*\" != *--dry-run* ]]; then printf '%s\\n' 'pbi:1'; fi\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('PBI_VERSION lookup')\n"
                "    print('PBI_VERSION definition')\n"
                "    print('pbi entrypoint')\n"
                "    print('pbi configuration')\n"
                "    print('pbi tests')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('pbi:1')\n"
                "    print('1970-01-01T00:00')\n"
                "    print('127.0.0.1:3080')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is PBI_VERSION?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("pbi: no source location contains the queried symbol", result.stderr)
        self.assertEqual(result.stderr, "pbi: no source locations found\n")

    def test_default_query_planner_signal_emits_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            tmpdir = directory / "tmp"
            tmpdir.mkdir()
            env["TMPDIR"] = str(tmpdir)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import signal, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "    while True: time.sleep(0.1)\n"
                "raise SystemExit(2)\n"
            )
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = subprocess.run(
                [
                    "timeout",
                    "--kill-after=1s",
                    "2s",
                    str(self.fake_pbi(directory, directory / "probe")),
                    "where is the entrypoint",
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                cwd=ROOT,
                timeout=6,
            )
            elapsed = time.monotonic() - started
            self.assertEqual(list(tmpdir.iterdir()), [])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: planner timed out before producing a source answer\n")
        self.assertLess(elapsed, 5)

    def test_term_resistant_initial_planner_times_out_to_direct_bm25(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source = repo / "entrypoint.py"
            repo.mkdir()
            source.write_text("# filler\n" * 4 + "entrypoint\n")
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
                "    f.write('invoked')\n"
                "print('entrypoint.py:5')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, signal, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'w') as f:\n"
                "    json.dump({'argv': sys.argv[1:]}, f)\n"
                "if message.startswith('Convert the code question'):\n"
                "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "    while True: time.sleep(0.1)\n"
                "else:\n"
                "    raise SystemExit(2)\n"
            )
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where is the entrypoint", env=env, cwd=repo, binary=self.fake_pbi(directory, probe), timeout=4
            )
            elapsed = time.monotonic() - started
            self.assertTrue((directory / "probe-trace.json").exists())
            self.assertFalse(trace.exists(), "a fast-path success must not start the planner")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(elapsed, 3)
        self.assertEqual(result.stdout, "entrypoint.py:5\n")
        self.assertEqual(result.stderr, "")

    def test_term_resistant_refinement_planner_times_out_to_direct_bm25(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "if \"--dry-run\" not in sys.argv:\n"
                "    print(f'File: {os.getcwd()}/LICENSE, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, signal, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'a') as f:\n"
                "    print(json.dumps(message), file=f)\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('initial query')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    signal.signal(signal.SIGTERM, lambda *_: None)\n"
                "    while True: time.sleep(0.1)\n"
                "else:\n"
                "    raise SystemExit(2)\n"
            )
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe), timeout=4
            )
            elapsed = time.monotonic() - started
            planner_messages = [json.loads(line) for line in trace.read_text().splitlines()]
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(elapsed, 3)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: planner timed out before producing a source answer\n")
        self.assertNotIn("LICENSE:1", result.stdout + result.stderr)
        self.assertEqual(len(planner_messages), 2)
        self.assertTrue(planner_messages[0].startswith("Convert the code question"))
        self.assertTrue(planner_messages[1].startswith("Identify missing evidence"))

    def test_loads_cwd_dotenv_for_local_router_without_leaking_secret(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            for name in (
                "CLIPROXY_API_KEY",
                "OPENAI_API_KEY",
                "CLIPROXY_BASE_URL",
                "LOCAL_ROUTER_API_KEY",
                "LOCAL_ROUTER_BASEURL",
                "LOCAL_MODEL",
                "LLM_MODEL",
                "FALLBACK_MODEL",
            ):
                env.pop(name, None)
            (directory / ".env").write_text(
                "LOCAL_ROUTER_BASEURL=http://router.invalid/v1\n"
                "LOCAL_ROUTER_API_KEY=dummy-dotenv-key\n"
                "LLM_MODEL=dummy-primary\n"
                "FALLBACK_MODEL=dummy-fallback\n"
            )
            result = self.run_pbi("--message", "hello", env=env, cwd=directory, binary=self.fake_pbi(directory, directory / "probe"))
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(recorded["env"]["OPENAI_API_KEY"], "dummy-dotenv-key")
        self.assertEqual(recorded["env"]["OPENAI_API_URL"], "http://router.invalid/v1")
        self.assertEqual(recorded["env"]["MODEL_NAME"], "dummy-primary")
        self.assertNotIn("dummy-dotenv-key", result.stdout + result.stderr)

    def test_process_environment_overrides_cwd_dotenv(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            env.pop("CLIPROXY_API_KEY", None)
            env.pop("OPENAI_API_KEY", None)
            env.update(
                {
                    "LOCAL_ROUTER_BASEURL": "http://process.invalid/v1",
                    "LOCAL_ROUTER_API_KEY": "dummy-process-key",
                    "LLM_MODEL": "process-primary",
                    "FALLBACK_MODEL": "process-fallback",
                }
            )
            (directory / ".env").write_text(
                "LOCAL_ROUTER_BASEURL=http://dotenv.invalid/v1\n"
                "LOCAL_ROUTER_API_KEY=dummy-dotenv-key\n"
                "LLM_MODEL=dotenv-primary\n"
                "FALLBACK_MODEL=dotenv-fallback\n"
            )
            result = self.run_pbi("--message", "hello", env=env, cwd=directory, binary=self.fake_pbi(directory, directory / "probe"))
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(recorded["env"]["OPENAI_API_KEY"], "dummy-process-key")
        self.assertEqual(recorded["env"]["OPENAI_API_URL"], "http://process.invalid/v1")
        self.assertEqual(recorded["env"]["MODEL_NAME"], "process-primary")
        self.assertEqual(
            [provider["model"] for provider in json.loads(recorded["env"]["FALLBACK_PROVIDERS"])],
            ["process-primary", "process-fallback"],
        )

    def test_default_query_symbol_less_range_stays_a_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(f"#!/usr/bin/env bash\nprintf \"%s\\n\" \"File: {PBI}, Lines: 211-220\"\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "print(\"AI SDK Warning: System messages are risky.\")\n"
                "raise SystemExit(7)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe)
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertNotIn("pbi:211", result.stdout + result.stderr)

    def test_default_query_fails_closed_when_answer_has_no_usable_text(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$*\" == *--dry-run* ]]; then\n"
                "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-10'\n"
                "else\n"
                f"    printf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
                "fi\n"
                "printf '%s\\n' 'SEARCH_SENTINEL' >&2\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('entrypoint query')\n"
                "    print('PLANNER_SENTINEL', file=sys.stderr)\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('AI SDK Warning: System messages are risky.')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe)
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("pbi: no source locations found", result.stderr)
        self.assertNotIn("SEARCH_SENTINEL", result.stdout + result.stderr)
        self.assertNotIn("PLANNER_SENTINEL", result.stdout + result.stderr)

    def test_question_stamp_only_model_answer_fails_closed(self) -> None:
        # #12: a local-model "answer" that is only raw BM25 `path:1` stamps
        # (the model mirroring the candidate echo) must not count as success.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            (directory / "probe").write_text(
                f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-40'\n"
            )
            (directory / "probe").chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('entrypoint CLI parsing')\n"
                "    print('command dispatch match')\n"
                "    print('clap Subcommand derive')\n"
                "    print('persistence write callers')\n"
                "    print('result return formatting')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    for i in range(23):\n"
                "        print(f'agent/conversation_compression.py:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where",
                "is",
                "the",
                "entrypoint",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, directory / "probe"),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("location stamps", result.stderr)

    def test_default_query_mixed_compact_stamps_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-40'\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('entrypoint CLI parsing')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    for stamp in ('pbi:1', 'path:line', 'pbi:1', 'path:1', 'pbi:1',\n"
                "                  'path:1', 'pbi:1', 's:1', 's:1', 'LICENSE:1'):\n"
                "        print(stamp)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

    def test_default_query_real_path_stamp_only_answer_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            paths = (
                directory / "website/docs/developer-guide/trajectory-format.md",
                directory / "tui_gateway/server.py",
                directory / "apps/desktop/electron/main.ts",
                directory / "tools/delegate_tool.py",
            )
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("source\n")
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                + "\n".join(f"print('File: {path}, Lines: 1-40')" for path in paths)
                + "\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('AI SDK Warning: System messages are risky.')\n"
                "    raise SystemExit(7)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

    def test_default_query_spaced_and_punctuated_real_path_stamps_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            paths = (directory / "docs/user guide.md", directory / "src/foo+bar.rs")
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("source\n")
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                + "\n".join(f"print('File: {path}, Lines: 1-40')" for path in paths)
                + "\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "print('AI SDK Warning: System messages are risky.')\n"
                "raise SystemExit(7)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

    def test_question_spaced_relative_path_stamps_fail_closed(self) -> None:
        for answer in ("user guide.md:1", "user docs/foo.md:1"):
            with self.subTest(answer=answer), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                env, _ = self.fake_environment(directory)
                (directory / "probe").write_text(
                    "#!/usr/bin/env bash\n"
                    "if [[ \"$*\" == *--dry-run* ]]; then\n"
                    "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-40'\n"
                    "else\n"
                    f"    printf '%s\\n' 'File: {PBI}, Lines: 1-40'\n"
                    "fi\n"
                )
                (directory / "probe").chmod(0o755)
                fake_chat = directory / "probe-chat"
                fake_chat.write_text(
                    "#!/usr/bin/env python3\n"
                    "import sys\n"
                    "message = sys.argv[sys.argv.index('--message') + 1]\n"
                    "if message.startswith('Convert the code question'):\n"
                    "    print('entrypoint CLI parsing')\n"
                    "elif message.startswith('Identify missing evidence'):\n"
                    "    print('NONE')\n"
                    "else:\n"
                    f"    print({answer!r})\n"
                )
                fake_chat.chmod(0o755)
                result = self.run_pbi(
                    "where is the entrypoint",
                    env=env,
                    cwd=ROOT,
                    binary=self.fake_pbi(directory, directory / "probe"),
                )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")
            self.assertEqual(
                result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n"
            )

    def test_question_stamp_per_line_narrative_answer_still_succeeds(self) -> None:
        # #12: a real compact answer (narrative + citation) still prints, even
        # when it includes a `path:1`-style citation line.
        for answer in (
            "The entrypoint is pbi:1",
            "The entrypoint is ./pbi:1",
            "The tests are in test_pbi.py:1",
        ):
            with self.subTest(answer=answer), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                env, _ = self.fake_environment(directory)
                (directory / "probe").write_text(
                    "#!/usr/bin/env bash\n"
                    "if [[ \"$*\" == *--dry-run* ]]; then\n"
                    "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-40'\n"
                    "else\n"
                    f"    printf '%s\\n' 'File: {PBI}, Lines: 1-40'\n"
                    "fi\n"
                )
                (directory / "probe").chmod(0o755)
                fake_chat = directory / "probe-chat"
                fake_chat.write_text(
                    "#!/usr/bin/env python3\n"
                    "import sys\n"
                    "message = sys.argv[sys.argv.index('--message') + 1]\n"
                    "if message.startswith('Convert the code question'):\n"
                    "    print('entrypoint CLI parsing')\n"
                    "elif message.startswith('Identify missing evidence'):\n"
                    "    print('NONE')\n"
                    "else:\n"
                    f"    print({answer!r})\n"
                )
                fake_chat.chmod(0o755)
                result = self.run_pbi(
                    "where is the entrypoint",
                    env=env,
                    cwd=ROOT,
                    binary=self.fake_pbi(directory, directory / "probe"),
                )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, f"{answer}\n")

    def test_no_colon_warning_only_stdout_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$*\" == *--dry-run* ]]; then\n"
                "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-10'\n"
                "else\n"
                f"    printf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
                "fi\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'AI SDK Warning System messages are not supported'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is anything", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe)
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("pbi: no source locations found", result.stderr)
        self.assertNotIn("AI SDK Warning", result.stdout)

    def test_query_planning_system_message_warning_keeps_local_model_answer(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            (directory / "probe").write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$*\" == *--dry-run* ]]; then\n"
                "    printf '%s\\n' 'File: /missing/pbi, Lines: 1-10'\n"
                "else\n"
                f"    printf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
                "fi\n"
            )
            (directory / "probe").chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'a') as f:\n"
                "    print(json.dumps(sys.argv[1:]), file=f)\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('AI SDK Warning: System messages are risky.')\n"
                "    print('entrypoint CLI parsing')\n"
                "    print('command dispatch match')\n"
                "    print('clap Subcommand derive')\n"
                "    print('persistence write callers')\n"
                "    print('result return formatting')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "elif message.startswith('Answer the question'):\n"
                "    print('MODEL_ANSWER pbi:9')\n"
                "elif message.startswith('Review and compress') or message.startswith('Audit every'):\n"
                "    print('MODEL_ANSWER pbi:9')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, directory / "probe"))
            calls = [json.loads(line) for line in trace.read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "MODEL_ANSWER pbi:9\n")
        self.assertNotEqual(result.stdout, "pbi:1\n")
        self.assertTrue(any(call[call.index("--message") + 1].startswith("Answer the question") for call in calls))

    def test_nonzero_query_planning_warning_fails_closed_without_echoing_sentinels(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            (directory / "probe").write_text(
                f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
            )
            (directory / "probe").chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'AI SDK Warning: System messages are risky.'\n"
                "printf '%s\\n' 'PROMPT_SENTINEL SECRET_SENTINEL' >&2\n"
                "exit 7\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, directory / "probe"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertNotIn("PROMPT_SENTINEL", result.stdout + result.stderr)
        self.assertNotIn("SECRET_SENTINEL", result.stdout + result.stderr)

    def test_routes_primary_retries_then_fallback_and_forwards_args(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env, trace = self.fake_environment(Path(temporary))
            result = self.run_pbi("--message", "hello", "--json", env=env)
            self.assertTrue(trace.exists(), result.stderr)
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(
            recorded["argv"],
            [
                "--force-provider",
                "openai",
                "--model-name",
                PRIMARY,
                "--message",
                "hello",
                "--json",
            ],
        )
        configured = recorded["env"]
        self.assertTrue(configured["PROBE_BINARY_PATH"].endswith("/probe"))
        self.assertNotIn("/mise/shims/", configured["PROBE_BINARY_PATH"])
        self.assertEqual(configured["FORCE_PROVIDER"], "openai")
        self.assertEqual(configured["MODEL_NAME"], PRIMARY)
        self.assertEqual(configured["OPENAI_API_KEY"], "test-key")
        self.assertEqual(configured["OPENAI_API_URL"], BASE_URL)
        self.assertEqual(configured["LLM_BASE_URL"], BASE_URL)
        self.assertEqual(configured["MAX_RETRIES"], "3")
        self.assertGreaterEqual(int(configured["REQUEST_TIMEOUT"]), 1_700_000)
        self.assertGreaterEqual(
            int(configured["MAX_OPERATION_TIMEOUT"]), int(configured["REQUEST_TIMEOUT"]) * 5
        )
        providers = json.loads(configured["FALLBACK_PROVIDERS"])
        self.assertEqual([provider["model"] for provider in providers], [PRIMARY, FALLBACK])
        self.assertEqual(providers[0]["maxRetries"], 3)
        self.assertEqual(providers[1]["maxRetries"], 0)

    def test_bm25_search_delegates_to_probe_without_an_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env = os.environ.copy()
            env["HOME"] = temporary
            env.pop("CLIPROXY_API_KEY", None)
            env.pop("OPENAI_API_KEY", None)
            result = self.run_pbi(
                "search", "--bm25", "PBI_VERSION", "--format", "plain", "--max-results", "1", env=env, cwd=ROOT
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("File:", result.stdout)

    def test_bm25_search_defaults_to_a_small_result_set_without_an_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            env.pop("CLIPROXY_API_KEY", None)
            env.pop("OPENAI_API_KEY", None)
            probe = directory / "probe"
            self.record_probe_argv(probe)
            result = self.run_pbi("search", "--bm25", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, probe))
            argv = json.loads((directory / "probe-trace.json").read_text())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            argv,
            ["search", "--reranker", "bm25", "--timeout", "540", "--max-results", "8", "--", "PBI_VERSION"],
        )

    def test_bm25_search_keeps_probe_stdout_and_stderr_separate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'pbi:37'\n"
                "printf '%s\\n' 'Probe warning' >&2\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("search", "--bm25", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, probe))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:37\n")
        self.assertEqual(result.stderr, "Probe warning\n")

    def test_bm25_search_replays_non_timeout_failure_streams_and_status(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'pbi:37'\n"
                "printf '%s\\n' 'Probe warning' >&2\n"
                "exit 23\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("search", "--bm25", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, probe))
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stdout, "pbi:37\n")
        self.assertEqual(result.stderr, "Probe warning\n")

    def test_search_defaults_to_local_model_without_bert(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            self.record_probe_invocation(probe)
            result = self.run_pbi(
                "search", "SessionDB", "FTS5", "session", "search", env=env, binary=self.fake_pbi(directory, probe)
            )
            probe_recorded = json.loads((directory / "probe-trace.json").read_text())
            chat_recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(
            probe_recorded["argv"],
            [
                "search",
                "--timeout",
                "540",
                "--max-results",
                "8",
                "--ignore",
                "drafts",
                "--reranker",
                "bm25",
                "--format",
                "plain",
                "--dry-run",
                "--",
                "SessionDB FTS5 session search",
            ],
        )
        self.assertNotIn("ms-marco-minilm-l6", probe_recorded["argv"])
        self.assertEqual(
            chat_recorded["argv"][:5],
            ["--force-provider", "openai", "--model-name", PRIMARY, "--message"],
        )
        self.assertEqual(chat_recorded["argv"][6:8], ["--max-iterations", "1"])
        self.assertNotIn("--prompt", chat_recorded["argv"])
        self.assertEqual(chat_recorded["env"]["FORCE_PROVIDER"], "openai")
        self.assertEqual(chat_recorded["env"]["MODEL_NAME"], PRIMARY)
        self.assertEqual(chat_recorded["env"]["OPENAI_API_KEY"], "test-key")
        self.assertEqual(chat_recorded["env"]["OPENAI_API_URL"], BASE_URL)
        self.assertEqual(chat_recorded["env"]["MAX_RETRIES"], "3")
        self.assertEqual(
            [provider["model"] for provider in json.loads(chat_recorded["env"]["FALLBACK_PROVIDERS"])],
            [PRIMARY, FALLBACK],
        )

    def test_search_hides_mocked_bert_fallback_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"BERT reranker 'ms-marco-minilm-l6' is not available.\"\n"
                "printf '%s\\n' 'Falling back to BM25 ranking...'\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("search", "SessionDB", env=env, binary=self.fake_pbi(directory, probe))
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertNotIn("BERT reranker", result.stdout)
        self.assertNotIn("Falling back to BM25", result.stdout)
        self.assertEqual(
            recorded["argv"],
            [
                "--force-provider",
                "openai",
                "--model-name",
                PRIMARY,
                "--message",
                "Use Probe BM25 candidates to find SessionDB. Return only the best matching "
                "path:symbol or path:line locations; no narration.\n\n",
                "--max-iterations",
                "1",
            ],
        )

    def test_named_symbol_candidate_skips_stamp_diagnostic_without_api_key(self) -> None:
        symbol = "soft_delete_drawer"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text("\n".join(["# filler"] * 210) + f"\ndef {symbol}():\n    return True\n")
            env, trace = self.fake_environment(directory)
            for name in (
                "CLIPROXY_API_KEY",
                "OPENAI_API_KEY",
                "LOCAL_ROUTER_API_KEY",
            ):
                env.pop(name, None)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {source}, Lines: 211-212')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'pbi: model returned only BM25 location stamps; no source answer'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                symbol,
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:211\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "a verified BM25 location must not invoke stamp-producing chat")

    def test_search_prints_only_compact_locations(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                f"#!/usr/bin/env bash\n"
                f"printf '%s\\n' '- /repo ✓' '{PBI}:5' "
                "'AI SDK Warning: System messages can enable prompt injection.'\n"
                "printf '%s\\n' 'AI SDK Warning: System messages can enable prompt injection.' >&2\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("search", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, directory / "probe"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:5\n")
        self.assertEqual(result.stderr, "")

    def test_search_compact_stamp_fallback_fails_closed_without_token(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            sources = {
                name: repo / name for name in ("a.py", "b.rs", "c.md")
            }
            for source in sources.values():
                source.write_text("unrelated = True\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                + "\n".join(
                    f"print(\"File: {source}, Lines: 1-2\")"
                    for source in sources.values()
                )
                + "\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'a.py:1' 'b.rs:1' 'c.md:1'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find",
                "breaker-open",
                "receipt",
                "#927",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(
            result.stderr,
            "pbi: model returned only BM25 location stamps; no source answer\n",
        )
        self.assertNotIn("a.py:1", result.stdout + result.stderr)
        self.assertNotIn("b.rs:1", result.stdout + result.stderr)
        self.assertNotIn("c.md:1", result.stdout + result.stderr)

    def test_search_compact_stamp_fallback_recovers_distinctive_token(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            sources = {
                "a.py": repo / "a.py",
                "b.rs": repo / "b.rs",
                "c.md": repo / "c.md",
            }
            sources["a.py"].write_text("unrelated = True\n")
            sources["b.rs"].write_text("// breaker-open appears in a comment\nstate = breaker-open\n")
            sources["c.md"].write_text("unrelated\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                + "\n".join(
                    f"print(\"File: {source}, Lines: 1-2\")"
                    for source in sources.values()
                )
                + "\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'a.py:1' 'b.rs:1' 'c.md:1'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find",
                "breaker-open",
                "receipt",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "b.rs:2\n")
        self.assertEqual(result.stderr, "")

    def test_search_named_symbol_does_not_succeed_with_unrelated_file(self) -> None:
        # #8: a search whose query names a real symbol must not print an
        # unrelated compact location (wrong file) as success. The completed
        # location is only printed when its file actually contains the symbol.
        symbol = "test_first_api_call_reports_cache_hit_to_tui_callback"
        query = f"Locate {symbol} and callback signature"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            (repo / "other.py").write_text("def other():\n    return 0\n")

            # Model picks an unrelated file that lacks the named symbol -> fail closed.
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'other.py:5'\n")
            fake_chat.chmod(0o755)
            wrong = self.run_pbi(
                "search", query, env=env,
                cwd=repo, binary=self.fake_pbi(directory, directory / "probe"),
            )

            # Same repo, model points at the file that holds the symbol -> succeed.
            env2, _ = self.fake_environment(directory)
            fake_chat2 = directory / "probe-chat"
            fake_chat2.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'real.py:5'\n")
            fake_chat2.chmod(0o755)
            right = self.run_pbi(
                "search", query, env=env2,
                cwd=repo, binary=self.fake_pbi(directory, directory / "probe"),
            )
        self.assertEqual(wrong.returncode, 0, wrong.stderr)
        self.assertEqual(wrong.stdout, "real.py:1\n")
        self.assertEqual(wrong.stderr, "")
        self.assertEqual(right.returncode, 0, right.stderr)
        self.assertEqual(right.stdout, "real.py:1\n")
        self.assertEqual(right.stderr, "")

    def test_search_stamp_only_model_answer_fails_closed(self) -> None:
        # #12 r2: a search whose compacted stdout is only bare `path:1` stamps
        # (the model echoing the BM25 candidate set) must not report success.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'agent/conversation_compression.py:1' 'router/dispatch.rs:1'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "HERMES_TUI_RPC_TIMEOUT_MS",
                env=env,
                binary=self.fake_pbi(directory, directory / "probe"),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("location stamps", result.stderr)

    def test_search_stamp_only_echo_recovers_real_location_from_candidates(self) -> None:
        # #17: when a search answers with only BM25-style `path:1` stamps (the
        # model echoing the candidate set), pbi must recover a real location
        # from the candidate set already in hand instead of reporting the stamp
        # echo as the answer — and must not print the stamp sentence on stdout.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(
                "def diagnostic_busy_timeout_statistics():\n    return True\n"
            )

            # The natural-language query intentionally has no named symbol, so
            # this recovery test must invoke the post-chat path.
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {repo}/real.py, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'real.py:1'\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find the real implementation for diagnostic busy timeout statistics",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertNotIn("location stamps", result.stdout)

    def test_search_generic_probe_failure_recovers_candidates(self) -> None:
        symbol = "recover_search_from_candidates"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text(f"def {symbol}():\n    return True\n")
            candidate = f"File: {source}, Lines: 1-3"
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env python3\n" + f"print({candidate!r})\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf \"%s\\n\" \"connection reset\" >&2\n"
                "exit 23\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find",
                "the",
                "candidate",
                "implementation",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertNotIn("pbi: probe-chat failed", result.stdout + result.stderr)

    def test_search_generic_probe_failure_without_candidates_stays_failed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf \"%s\\n\" \"connection reset\" >&2\n"
                "exit 23\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find",
                "a",
                "missing",
                "source",
                env=env,
                cwd=directory,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 23)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe-chat failed\n")
        self.assertNotIn("connection reset", result.stdout + result.stderr)

    def test_search_api_error_recovers_named_symbol_from_candidates(self) -> None:
        # #22: an API-error payload must not hide an already-retrieved location
        # for a natural-language query; post-chat recovery returns the candidate.
        symbol = "ingest_receipt_accepts_cleanup_ids_from_legacy_wire_shape"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {repo}/real.py, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"error\": {\"code\": \"invalid_request\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search",
                "find the real implementation for accepting cleanup identifiers from an older wire format",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")

    def test_search_api_error_recovers_string_only_named_symbol_outside_bm25_range(self) -> None:
        symbol = "IPv6"
        query = "MCP Host allowlist IPv6 loopback bracketed authority"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source = repo / "src" / "api" / "mcp_ipv6_tests.rs"
            source.parent.mkdir(parents=True)
            lines = ["// filler"] * 26
            lines.append('const FIRST: &str = "IPv6 Host with bracketed authority was rejected";')
            lines.extend(["// filler"] * 15)
            lines.append('const SECOND: &str = "portless IPv6 Host was accepted";')
            lines.extend(["// filler"] * 16)
            lines.append('const THIRD: &str = "non-loopback IPv6 Host was accepted";')
            source.write_text("\n".join(lines) + "\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"status\": \"error\"}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp_ipv6_tests.rs:27\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "candidate recovery must skip Probe Chat")

    def test_search_keeps_in_range_non_declaration_symbol_hit(self) -> None:
        symbol = "PBI_VERSION"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "pbi"
            source.write_text("\n".join(["# filler"] * 4 + [f"{symbol}=\"0.1.0\"", "# filler"]) + "\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-10\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch \"$PBI_TEST_TRACE\"\n"
                "printf \"%s\\n\" \"{\\\"error\\\": {\\\"code\\\": \\\"invalid_request\\\"}}\"\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:5\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "an in-range symbol hit must skip Probe Chat")

    def test_search_handles_leading_dash_candidate_filename(self) -> None:
        symbol = "OptionLike"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "--help"
            source.write_text(f"class {symbol}:\n    pass\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source.name}, Lines: 1-2\")\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "--help:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "an in-range candidate must skip Probe Chat")

    def test_search_prefers_named_symbol_over_all_caps_acronyms(self) -> None:
        query = "daemon_mcp_listen_port HTTP MCP session roots project isolation"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src" / "api"
            source_dir.mkdir(parents=True)
            unrelated = source_dir / "mcp.rs"
            unrelated.write_text("const HTTP_REQUEST_TIMEOUT: u64 = 30;\n")
            definition = source_dir / "mcp_tests.rs"
            definition.write_text(
                "async fn daemon_mcp_listen_port_fails_closed_when_daemon_down() {}\n"
            )
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {unrelated}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch \"$PBI_TEST_TRACE\"\n"
                "sleep 30\n"
            )
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
            self.assertFalse(trace.exists(), "a verified named symbol must skip Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp_tests.rs:1\n")
        self.assertEqual(result.stderr, "")
        self.assertLess(elapsed, 5)

    def test_search_ignores_commented_prefix_definition_before_real_definition(self) -> None:
        query = "daemon_mcp_listen_port HTTP MCP session roots project isolation"
        comment_cases = {
            "leading block":
                "/*\nfn daemon_mcp_listen_port_fake_comment() {}\n*/\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "leading documentation block":
                "/* documentation\nfn daemon_mcp_listen_port_fake_comment() {}\n*/\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "trailing slash comment":
                "let x = 1; // daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "trailing hash comment":
                "x = 1 # daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "mid-line hash include text":
                "value = 1 #include daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "mid-line hash attribute text":
                "value = 1 #[derive] daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "mid-line hash inner attribute text":
                "value = 1 #![allow] daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "leading hash include prefix":
                "#included daemon_mcp_listen_port_fake_comment\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
            "mid-line block comment":
                "code /* daemon_mcp_listen_port_fake_comment\n"
                "fn daemon_mcp_listen_port_fake_comment() {}\n*/\n"
                "const HTTP_REQUEST_TIMEOUT: u64 = 30;\n",
        }
        for comment_case, comment_text in comment_cases.items():
            with self.subTest(comment_case=comment_case), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                repo = directory / "repo"
                source_dir = repo / "src" / "api"
                source_dir.mkdir(parents=True)
                comment_file = repo / "aaa_comment.rs"
                comment_file.write_text(comment_text)
                definition = source_dir / "mcp_tests.rs"
                definition.write_text(
                    "async fn daemon_mcp_listen_port_fails_closed_when_daemon_down() {}\n"
                )
                env, trace = self.fake_environment(directory)
                env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
                probe = directory / "probe"
                probe.write_text(
                    "#!/usr/bin/env python3\n"
                    f"print(\"File: {comment_file}, Lines: 1-4\")\n"
                )
                probe.chmod(0o755)
                fake_chat = directory / "probe-chat"
                fake_chat.write_text(
                    "#!/usr/bin/env python3\n"
                    "import os, time\n"
                    "open(os.environ[\"PBI_TEST_TRACE\"], \"w\").close()\n"
                    "time.sleep(30)\n"
                )
                fake_chat.chmod(0o755)
                started = time.monotonic()
                result = self.run_pbi(
                    "search", *query.split(), env=env, cwd=repo,
                    binary=self.fake_pbi(directory, probe), timeout=5,
                )
                elapsed = time.monotonic() - started
                self.assertFalse(trace.exists(), "a commented prefix must not skip Probe Chat")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "src/api/mcp_tests.rs:1\n")
            self.assertEqual(result.stderr, "")
            self.assertLess(elapsed, 5)

    def test_search_keeps_line_leading_hash_directives_as_code(self) -> None:
        query = "daemon_mcp_listen_port HTTP MCP session roots project isolation"
        real_cases = {
            "line-leading include": "#include daemon_mcp_listen_port_real_hit\n",
            "line-leading attribute": "#[derive] daemon_mcp_listen_port_real_hit\n",
        }
        for real_case, source_text in real_cases.items():
            with self.subTest(real_case=real_case), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                repo = directory / "repo"
                repo.mkdir()
                source = repo / "real.py"
                source.write_text(source_text)
                env, trace = self.fake_environment(directory)
                env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
                probe = directory / "probe"
                probe.write_text(
                    "#!/usr/bin/env python3\n"
                    f"print(\"File: {source}, Lines: 1-1\")\n"
                )
                probe.chmod(0o755)
                fake_chat = directory / "probe-chat"
                fake_chat.write_text(
                    "#!/usr/bin/env bash\n"
                    "touch \"$PBI_TEST_TRACE\"\n"
                    "sleep 30\n"
                )
                fake_chat.chmod(0o755)
                result = self.run_pbi(
                    "search", *query.split(), env=env, cwd=repo,
                    binary=self.fake_pbi(directory, probe), timeout=5,
                )
                self.assertFalse(trace.exists(), "a line-leading directive must skip Probe Chat")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "real.py:1\n")
            self.assertEqual(result.stderr, "")

    def test_search_accepts_later_uncommented_symbol_hit_inside_bm25_range(self) -> None:
        symbol = "search_fallback_locations"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text(
                "\n".join(
                    [
                        f"{symbol} = \"before\"",
                        "# filler",
                        "# filler",
                        "# filler",
                        f"{symbol} = \"inside\"",
                    ]
                )
                + "\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 5-5\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch \"$PBI_TEST_TRACE\"\n"
                "printf \"%s\\n\" \"{\\\"error\\\": {\\\"code\\\": \\\"invalid_request\\\"}}\"\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            self.assertFalse(trace.exists(), "an in-range hit must skip Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:5\n")
        self.assertEqual(result.stderr, "")

    def test_search_recovers_named_symbol_definition_outside_bm25_snippet(self) -> None:
        query = "WriteSpool _replay_operation"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text(
                "\n".join(
                    [
                        "# module filler",
                        "# module filler",
                        "# definitions are outside the BM25 snippet",
                        "class WriteSpool:",
                        "    def __init__(self):",
                        "        self.value = 0",
                        "",
                        "def _replay_operation(spool):",
                        "    return spool.value",
                        "",
                        "# BM25 snippet contains call sites, not definitions",
                        "def helper(spool):",
                        "    return spool._replay_operation()",
                        "",
                        "spool = WriteSpool()",
                        "spool._replay_operation()",
                    ]
                )
                + "\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 11-12\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch \"$PBI_TEST_TRACE\"\n"
                "printf \"%s\\n\" \"{\\\"error\\\": {\\\"code\\\": \\\"invalid_request\\\"}}\"\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:8\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "verified definition should skip Probe Chat")

    def test_search_recovers_shorter_named_symbol_from_dual_symbol_candidates(self) -> None:
        query = "WriteSpool _replay_operation"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text(
                "class WriteSpool:\n"
                "    def _replay_operation(self):\n"
                "        return True\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch $PBI_TEST_TRACE\n"
                "exit 23\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "verified candidate should skip Probe Chat")

    def test_search_fails_closed_when_dual_symbols_are_absent(self) -> None:
        query = "WriteSpool _replay_operation"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text("class Other:\n    pass\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\n' '{\"error\": {\"code\": \"invalid_request\", \"message\": \"model not found\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr.count("\n"), 1)
        self.assertEqual(result.stderr, "pbi: no source location contains the queried symbol\n")
        self.assertNotIn("model not found", result.stderr)
        self.assertNotIn('{\"error\":', result.stderr)
    def test_search_does_not_recover_capitalized_prose_as_a_named_symbol(self) -> None:
        query = "Locate MissingClass _definitely_missing"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "prose.py"
            source.write_text("# Locate is prose, not a requested symbol\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf \"%s\\n\" \"{\\\"error\\\": {\\\"code\\\": \\\"invalid_request\\\"}}\"\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", *query.split(), env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr.count("\n"), 1)
        self.assertEqual(result.stderr, "pbi: no source location contains the queried symbol\n")
        self.assertNotIn("prose.py:1", result.stdout + result.stderr)

    def test_search_does_not_claim_absence_when_rg_is_missing(self) -> None:
        symbol = "present_named_symbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            candidate = repo / "candidate.py"
            candidate.write_text("def unrelated():\n    return True\n")
            env, _ = self.fake_environment(directory)
            tool_path = directory / "tool-path"
            tool_path.mkdir()
            for name in ("bash", "python3", "env", "sleep", "timeout", "setsid", "sh", "mktemp", "rm", "realpath", "grep", "awk", "sort", "cut", "sed", "head", "readlink"):
                command = shutil.which(name)
                if command:
                    (tool_path / name).symlink_to(command)
            env["PATH"] = os.pathsep.join((str(directory), str(tool_path)))
            node = directory / "node"
            node.write_text("#!/usr/bin/env bash\nprintf '%s\n' '[]'\n")
            node.chmod(0o755)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {candidate}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            started = time.monotonic()
            result = self.run_pbi(
                "search", "Locate", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertNotIn("pbi: no source location contains the queried symbol", result.stderr)
        self.assertIn("pbi: probe-chat timed out answering the question", result.stderr)
        self.assertLess(elapsed, 5)

    def test_search_does_not_claim_absence_when_rg_fails(self) -> None:
        symbol = "present_named_symbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            candidate = repo / "candidate.py"
            candidate.write_text("def unrelated():\n    return True\n")
            env, _ = self.fake_environment(directory)
            rg = directory / "rg"
            rg.write_text("#!/usr/bin/env bash\nexit 2\n")
            rg.chmod(0o755)
            node = directory / "node"
            node.write_text("#!/usr/bin/env bash\nprintf '%s\n' '[]'\n")
            node.chmod(0o755)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {candidate}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            started = time.monotonic()
            result = self.run_pbi(
                "search", "Locate", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertNotIn("pbi: no source location contains the queried symbol", result.stderr)
        self.assertIn("pbi: probe-chat timed out answering the question", result.stderr)
        self.assertLess(elapsed, 5)

    def test_search_fails_closed_when_named_symbol_is_absent(self) -> None:
        symbol = "definitely_missing_search_symbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text("def unrelated():\n    return True\n")
            env, trace = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\n" "touch \"$PBI_TEST_TRACE\"\n" "sleep 30\n")
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", "Locate", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, directory / "probe"), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source location contains the queried symbol\n")
        self.assertFalse(trace.exists(), "an absent named symbol must not invoke Probe Chat")
        self.assertLess(elapsed, 5)

    def test_search_skips_hanging_chat_when_candidates_contain_named_symbol(self) -> None:
        symbol = "rest_response_prefers_created_ids_when_both_fields_exist"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {repo}/real.py, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.time()
            result = self.run_pbi(
                "search", f"Locate {symbol}", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.time() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertLess(elapsed, 5)

    def test_search_probe_timeout_recovers_compact_candidates_without_named_symbol(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text("# breaker-open appears in a comment first\nstate = breaker-open\n")
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-2\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", "find", "the", "breaker-open", "implementation",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=5,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:2\n")
        self.assertEqual(result.stderr, "")

    def test_search_probe_timeout_recovers_issue_number_after_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "real.py"
            source.write_text("# 927 appears in a comment first\nissue = 927\n")
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-2\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", "find", "issue", "#927", "implementation",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=5,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:2\n")
        self.assertEqual(result.stderr, "")

    def test_search_probe_timeout_rejects_stamp_only_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "stamp.py"
            source.write_text("unrelated = True\n")
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(\"File: {source}, Lines: 1-1\")\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", "find", "breaker-open", "receipt", "#927",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=5,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe-chat timed out answering the question\n")

    def test_search_hang_fails_closed_when_candidates_lack_named_symbol(self) -> None:
        # #22: a timed-out search must not turn unrelated BM25 candidates into success.
        symbol = "ingest_receipt_accepts_cleanup_ids_from_legacy_wire_shape"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(
                "\n".join(
                    ["def unrelated():", "    return True"]
                    + ["# filler"] * 17
                    + [f"# {symbol} is outside the candidate range"]
                )
                + "\n"
            )
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {repo}/real.py, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.time()
            result = self.run_pbi(
                "search", f"Locate {symbol}", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=20,
            )
            elapsed = time.time() - started
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertLess(elapsed, 10, "hung search probe-chat must be killed, not hang pbi")
        self.assertIn("pbi: probe-chat timed out answering the question", result.stderr)

    def test_search_probe_timeout_emits_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\ntimeout --kill-after=1s 0.1s sleep 30\n")
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", "breaker_open", env=env, cwd=ROOT,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 124)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe search timed out\n")
        self.assertLess(elapsed, 3)

    def test_bm25_search_probe_timeout_emits_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\ntimeout --kill-after=1s 0.1s sleep 30\n")
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", "--bm25", "breaker_open", env=env, cwd=ROOT,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 124)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: probe search timed out\n")
        self.assertLess(elapsed, 3)

    def test_search_falls_back_to_absolute_retrieved_location_from_symlinked_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            cwd = directory / "repo"
            cwd.symlink_to(ROOT, target_is_directory=True)
            env["PWD"] = str(cwd)
            probe = directory / "probe"
            probe.write_text(
                f"#!/usr/bin/env bash\nprintf '%s\\n' '{PBI}:37'\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'The model only returned narration.'\n"
                "printf '%s\\n' 'AI SDK Warning: ignored.' >&2\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "search", "PBI_VERSION", env=env, cwd=cwd, binary=self.fake_pbi(directory, probe)
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:37\n")
        self.assertEqual(result.stderr, "")

    def test_search_injects_a_long_default_timeout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            self.record_probe_argv(probe)
            result = self.run_pbi(
                "search", "PBI_VERSION", env=env, binary=self.fake_pbi(directory, probe)
            )
            argv = json.loads((directory / "probe-trace.json").read_text())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:5\n")
        self.assertEqual(
            argv[:9],
            ["search", "--timeout", "540", "--max-results", "8", "--ignore", "drafts", "--reranker", "bm25"],
        )

    def test_search_preserves_a_caller_timeout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            self.record_probe_argv(probe)
            result = self.run_pbi(
                "search",
                "PBI_VERSION",
                "--timeout",
                "12",
                env=env,
                binary=self.fake_pbi(directory, probe),
            )
            argv = json.loads((directory / "probe-trace.json").read_text())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:5\n")
        self.assertEqual(
            argv,
            [
                "search",
                "--timeout",
                "12",
                "--max-results",
                "8",
                "--ignore",
                "drafts",
                "--reranker",
                "bm25",
                "--format",
                "plain",
                "--dry-run",
                "--",
                "PBI_VERSION",
            ],
        )

    def test_search_combines_unquoted_words_into_one_pattern(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            self.record_probe_argv(probe)
            result = self.run_pbi(
                "search",
                "SessionDB",
                "FTS5",
                "session",
                "search",
                env=env,
                binary=self.fake_pbi(directory, probe),
            )
            argv = json.loads((directory / "probe-trace.json").read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(
            argv,
            [
                "search",
                "--timeout",
                "540",
                "--max-results",
                "8",
                "--ignore",
                "drafts",
                "--reranker",
                "bm25",
                "--format",
                "plain",
                "--dry-run",
                "--",
                "SessionDB FTS5 session search",
            ],
        )
        self.assertNotIn("FTS5", argv)

    def test_defaults_probe_folder_to_the_calling_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            codebase = directory / "codebase"
            codebase.mkdir()
            result = self.run_pbi("--message", "hello", env=env, cwd=codebase)
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(recorded["env"]["ALLOWED_FOLDERS"], str(codebase))

    def test_fails_closed_when_probe_reports_a_json_api_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"error\": {\"code\": \"invalid_request\", \"message\": \"model not found\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", "--json", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("probe-chat reported an API error", result.stderr)
        self.assertIn("status=invalid_request", result.stderr)
        self.assertNotIn('"message":"model not found"', result.stdout + result.stderr)
        self.assertNotIn('{"error":', result.stdout + result.stderr)

    def test_api_error_diagnostic_includes_safe_request_id(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"error\": {\"code\": \"invalid_request\", \"message\": \"model not found\", \"Authorization\": \"Bearer sk-secret\"}, \"request_id\": \"req_abc\", \"api_key\": \"sk-secret\"}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("status=invalid_request", result.stderr)
        self.assertIn("request=req_abc", result.stderr)
        self.assertNotIn('{"error":', result.stderr)
        self.assertNotIn("Authorization", result.stderr)
        self.assertNotIn("api_key", result.stderr)
        self.assertNotIn("sk-secret", result.stderr)
        self.assertNotIn("model not found", result.stderr)

    def test_api_error_diagnostic_prefers_nested_request_id_over_conflicting_root_id(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"id\":\"response_123\",\"error\":{\"code\":\"invalid_request\",\"request_id\":\"req_real\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("status=invalid_request", result.stderr)
        self.assertIn("request=req_real", result.stderr)
        self.assertNotIn("request=response_123", result.stderr)

    def test_api_error_diagnostic_prefers_error_json_in_mixed_records(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"status\":\"completed\",\"id\":\"resp_123\"}'\n"
                "printf '%s\\n' '{\"error\":{\"code\":\"invalid_request\",\"requestId\":\"req_real\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("status=invalid_request", result.stderr)
        self.assertIn("request=req_real", result.stderr)
        self.assertNotIn("status=completed", result.stderr)
        self.assertNotIn("request=resp_123", result.stderr)

    def test_api_error_detector_handles_rate_limit_error_in_mixed_records(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"status\":\"completed\",\"id\":\"resp_123\"}'\n"
                "printf '%s\\n' '{\"error\":{\"code\":\"rate_limit_exceeded\",\"requestId\":\"req_real\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("status=rate_limit_exceeded", result.stderr)
        self.assertIn("request=req_real", result.stderr)
        self.assertNotIn('{"error":', result.stderr)
        self.assertNotIn("request=resp_123", result.stderr)

    def test_api_error_diagnostic_omits_missing_request_id(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' '{\"error\": {\"code\": \"invalid_request\", \"message\": \"model not found\"}}'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("--message", "hello", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("status=invalid_request", result.stderr)
        self.assertNotIn("request=", result.stderr)
        self.assertNotIn("model not found", result.stderr)

    def test_chat_hang_fails_closed_in_bounded_time(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.time()
            result = self.run_pbi("--message", "hello", env=env, timeout=20)
            elapsed = time.time() - started
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertLess(elapsed, 10, "hung probe-chat must be killed, not hang pbi")
        self.assertIn("timed out answering the question", result.stderr)

    def test_debug_config_is_redacted_and_does_not_launch_agent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env, trace = self.fake_environment(Path(temporary))
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"primary_model={PRIMARY}", result.stdout)
        self.assertIn(f"fallback_model={FALLBACK}", result.stdout)
        self.assertIn(f"base_url={BASE_URL}", result.stdout)
        self.assertIn("max_retries=3", result.stdout)
        self.assertIn("search_timeout_seconds=540", result.stdout)
        self.assertIn("search_default=local_model", result.stdout)
        self.assertIn("search_bm25_opt_in=--bm25", result.stdout)
        self.assertIn("api_key=[REDACTED]", result.stdout)


    def test_config_toml_primary_model_is_used_without_model_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text('primary_model = "spark"\n')
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=spark", result.stdout)


    def test_config_toml_rejects_relative_xdg_config_home(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            home = directory / "home"
            home_config = home / ".config" / "pbi" / "config.toml"
            home_config.parent.mkdir(parents=True)
            home_config.write_text('primary_model = "spark"\n')
            for xdg_config_home in (".", "relative/config"):
                relative_config = directory / xdg_config_home / "pbi" / "config.toml"
                relative_config.parent.mkdir(parents=True, exist_ok=True)
                relative_config.write_text('primary_model = "shadow"\n')
                env["HOME"] = str(home)
                env["XDG_CONFIG_HOME"] = xdg_config_home
                result = self.run_pbi("--debug-config", env=env, cwd=directory)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("primary_model=spark", result.stdout)
                self.assertNotIn("primary_model=shadow", result.stdout)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")


    def test_config_toml_multiline_string_does_not_shadow_primary_model(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                'primary_model = "spark"\n'
                'description = """\n'
                'primary_model = "shadow"\n'
                '"""\n'
            )
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"primary_model={PRIMARY}", result.stdout)
        self.assertNotIn("primary_model=shadow", result.stdout)


    def test_config_toml_comments_do_not_trigger_multiline_string_guard(self) -> None:
        cases = (
            (
                'primary_model = "spark"\n'
                '# example: description = """\n'
                '# primary_model = "shadow"\n'
                '# """\n'
            ),
            (
                'primary_model = "spark"\n'
                '# example: description = ' + (chr(39) * 3) + '\n'
                '# primary_model = "shadow"\n'
                '# ' + (chr(39) * 3) + '\n'
            ),
            (
                'primary_model = "spark" # multiline example: """\n'
            ),
            (
                'primary_model = "spark" # multiline example: ' + (chr(39) * 3) + '\n'
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            for config_text in cases:
                with self.subTest(config_text=config_text):
                    config_path.write_text(config_text)
                    result = self.run_pbi("--debug-config", env=env)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("primary_model=spark", result.stdout)
                    self.assertNotIn("primary_model=shadow", result.stdout)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")


    def test_config_toml_ordinary_tables_are_ignored(self) -> None:
        cases = (
            (
                'primary_model = "spark"\n'
                '[provider]\n'
                'primary_model = "shadow"\n'
            ),
            (
                'model = "spark"\n'
                '[provider]\n'
                'model = "shadow"\n'
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            for config_text in cases:
                with self.subTest(config_text=config_text):
                    config_path.write_text(config_text)
                    result = self.run_pbi("--debug-config", env=env)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f"primary_model={PRIMARY}", result.stdout)
                    self.assertNotIn("primary_model=shadow", result.stdout)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")


    def test_config_toml_unsafe_multiline_and_array_tables_are_ignored(self) -> None:
        cases = (
            (
                'primary_model = "spark"\n'
                'description = """\n'
                'escaped ' + chr(92) + '""" delimiter\n'
                'primary_model = "shadow"\n'
                '"""\n'
            ),
            (
                'primary_model = "spark"\n'
                "description = '''\n"
                'primary_model = "shadow"\n'
                "'''\n"
            ),
            (
                'primary_model = "spark"\n'
                '"description" = """\n'
                'primary_model = "shadow"\n'
                '"""\n'
            ),
            (
                'primary_model = "spark"\n'
                'description = ["""\n'
                'primary_model = "shadow"\n'
                '"""]\n'
            ),
            (
                'model = "spark"\n'
                '"description" = ' + (chr(39) * 3) + '\n'
                'model = "shadow"\n'
                + (chr(39) * 3) + '\n'
            ),
            (
                'model = "spark"\n'
                'description = [' + (chr(39) * 3) + '\n'
                'model = "shadow"\n'
                + (chr(39) * 3) + ']\n'
            ),
            (
                'primary_model = "spark"\n'
                '[[providers]]\n'
                'primary_model = "shadow"\n'
            ),
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            for config_text in cases:
                with self.subTest(config_text=config_text):
                    config_path.write_text(config_text)
                    result = self.run_pbi("--debug-config", env=env)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f"primary_model={PRIMARY}", result.stdout)
                    self.assertNotIn("primary_model=shadow", result.stdout)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")


    def test_missing_or_empty_config_toml_uses_compiled_in_primary_model(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            missing_result = self.run_pbi("--debug-config", env=env)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text("")
            empty_result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        for result in (missing_result, empty_result):
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"primary_model={PRIMARY}", result.stdout)

    def test_config_toml_does_not_override_llm_model(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text('primary_model = "spark"\n')
            env["LLM_MODEL"] = "from-env"
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=from-env", result.stdout)

    def test_pbi_config_file_overrides_xdg_config_toml(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            xdg_path = directory / ".config" / "pbi" / "config.toml"
            xdg_path.parent.mkdir(parents=True)
            xdg_path.write_text('primary_model = "xdg"\n')
            override_path = directory / "custom" / "model.toml"
            override_path.parent.mkdir()
            override_path.write_text('primary_model = "override"\n')
            env["PBI_CONFIG_FILE"] = str(override_path)
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=override", result.stdout)

    def test_api_key_diagnostic_names_environment_only(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            for name in ("LOCAL_ROUTER_API_KEY", "CLIPROXY_API_KEY", "OPENAI_API_KEY"):
                env.pop(name, None)
            result = self.run_pbi("--message", "hello", env=env, cwd=directory)
        self.assertEqual(result.returncode, 78)
        self.assertEqual(
            result.stderr,
            "pbi: set LOCAL_ROUTER_API_KEY, CLIPROXY_API_KEY, or OPENAI_API_KEY in the environment\n",
        )

    def test_config_toml_honors_xdg_config_home(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            home = directory / "home"
            xdg_config_home = directory / "xdg-config"
            home.mkdir()
            config_path = xdg_config_home / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text('primary_model = "spark"\n')
            env["HOME"] = str(home)
            env["XDG_CONFIG_HOME"] = str(xdg_config_home)
            result = self.run_pbi("--debug-config", env=env, cwd=directory)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=spark", result.stdout)

    def test_config_toml_ignores_unknown_keys_without_discarding_model(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                'primary_model = "spark"\n'
                'description = "cost #1"\n'
                "timeout = 1\n"
                "tags = [\n"
                "  \"one\"\n"
                "]\n"
            )
            result = self.run_pbi("--debug-config", env=env, cwd=directory)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=spark", result.stdout)


    def test_fails_closed_with_diagnostic_when_probe_chat_cannot_launch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            # Non-executable probe-chat on PATH: execve fails, bash returns exit 126.
            (directory / "probe-chat").write_text(
                "#! /usr/bin/env bash\necho should-not-run\n"
            )
            (directory / "probe-chat").chmod(0o644)
            # Drop PATH entries that carry a real executable probe-chat so the non-exec
            # fake is what command -v resolves; keep node (pbi config) and core utils.
            node_bin = os.path.dirname(shutil.which("node") or "/usr/bin/node")
            env["PATH"] = f"{directory}:{node_bin}:/usr/bin:/bin"
            result = self.run_pbi("--message", "hello", env=env, cwd=ROOT)
        self.assertEqual(result.returncode, 126, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn("failed to launch", result.stderr)
        self.assertIn("126", result.stderr)
        self.assertNotIn("probe-chat reported an API error", result.stderr)
        self.assertNotIn("probe-chat failed", result.stderr)


    def test_default_query_bm25_fast_path_requires_append_audit_co_signal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            short_definition = source_dir / "store.rs"
            plan_audit = source_dir / "pipeline_tests_post_exec_audit.rs"
            long_definition = source_dir / "review_cmd_dirty_tree.rs"
            short_definition.write_text("// filler\n" * 204 + "pub fn append_entry() {}\n")
            plan_audit.write_text("// filler\n" * 44 + "fn should_audit_repo_tracked_writes_for_plan_task_type() {}\n")
            long_definition.write_text("// filler\n" * 124 + "pub(super) fn append_repo_write_audit_finding() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                f"if query == 'appending':\n    print('{plan_audit}:45')\n    print('{long_definition}:1')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "appending", "review", "audit", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/review_cmd_dirty_tree.rs:125\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("appending", probe_queries)
        self.assertNotIn("audit", probe_queries)
        self.assertNotIn("append", probe_queries)

    def test_default_query_bm25_fast_path_requires_full_hyphen_compound_on_cited_line(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            alias_definition = source_dir / "session_display_alias.rs"
            target = source_dir / "worktree_reclaim_tests.rs"
            alias_definition.write_text("// filler\n" * 25 + "pub(crate) fn alias_for_display_session() {}\n")
            target.write_text("// filler\n" * 44 + "fn worktree_write_lock_reclaims_terminal_session_after_holder_crash() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                f"if query == 'late_alias': print('{alias_definition}:26')\n"
                f"elif query == 'lock_reclaim': print('{target}:45')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "late-alias", "lock-reclaim", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/worktree_reclaim_tests.rs:45\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("late_alias", probe_queries)
        self.assertIn("lock_reclaim", probe_queries)
        self.assertNotIn("reclaim", probe_queries)
        self.assertNotIn("alias", probe_queries)

    def test_default_query_bm25_fast_path_expands_compound_miss_within_cited_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            display_alias = source_dir / "session_display_alias.rs"
            alias_race = source_dir / "alias_race.rs"
            display_alias.write_text("// filler\n" * 25 + "pub(crate) fn alias_for_display_session() {}\n")
            lines = ["// filler"] * 87
            lines.append("fn rebinds_when_alias_appears_after_wait_starts() {}")
            lines.extend(["// filler"] * (195 - len(lines)))
            lines.append('const LATE_ALIAS_NOTE: &str = "late alias must keep wrapper as an alias and must not get the fix result";')
            alias_race.write_text("\n".join(lines) + "\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                f"if query in ('late_alias', 'lock_reclaim'):\n    print('{display_alias}:26')\n    print('{alias_race}:88')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "late-alias", "lock-reclaim", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/alias_race.rs:196\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("late_alias", probe_queries)
        self.assertIn("lock_reclaim", probe_queries)
        self.assertNotIn("reclaim", probe_queries)
        self.assertNotIn("alias", probe_queries)

    def test_default_query_bm25_fast_path_recovers_remaining_file_footer(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            display_alias = source_dir / "session_display_alias.rs"
            alias_race = source_dir / "session_cmds_tests_tail_wait_resume_wrapper_alias_race.rs"
            tripwire = repo / "crates/foo/bar.rs"
            tripwire.parent.mkdir(parents=True)
            tripwire.write_text("fn late_alias_tripwire() {}\n")
            for index in range(20):
                unrelated = repo / f"crates/foo/unrelated_{index}.rs"
                unrelated.write_text("fn unrelated() {}\n")
            display_alias.write_text("// filler\n" * 25 + "pub(crate) fn alias_for_display_session() {}\n")
            lines = ["// filler"] * 87
            lines.append("fn rebinds_when_alias_appears_after_wait_starts() {}")
            lines.extend(["// filler"] * (195 - len(lines)))
            lines.append('const LATE_ALIAS_NOTE: &str = "late alias must keep wrapper as an alias and must not get the fix result";')
            alias_race.write_text("\n".join(lines) + "\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                f"if query in ('late_alias', 'lock_reclaim'):\n"
                f"    print('File: {display_alias}, Lines: 26-66')\n"
                "    print('Found 2 search results')\n"
                "    print('Remaining files not shown:')\n"
                "    print('  patterns/pr-bot/PATTERN.md <2> <17>')\n"
                "    print('  src/session_cmds_tests_tail_wait_resume_wrapper_alias_race.rs <2> <7>')\n"
                f"    print('  {tripwire.relative_to(repo)} <2> <7>')\n"
                "    for index in range(20): print(f'  crates/foo/unrelated_{index}.rs <1> <1>')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "late-alias", "lock-reclaim", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/session_cmds_tests_tail_wait_resume_wrapper_alias_race.rs:196\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("late_alias", probe_queries)
        self.assertIn("lock_reclaim", probe_queries)
        self.assertNotIn("alias", probe_queries)
        self.assertNotIn("reclaim", probe_queries)

    def test_default_query_bm25_fast_path_recovers_remaining_file_footer_for_append_audit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            store = source_dir / "store.rs"
            target = source_dir / "review_cmd_dirty_tree.rs"
            store.write_text("// filler\n" * 204 + "pub fn append_entry() {}\n")
            target.write_text("// filler\n" * 124 + "pub(super) fn append_repo_write_audit_finding() {}\n")
            for index in range(20):
                (source_dir / f"review_cmd_foo_{index}.rs").write_text("fn unrelated_review() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                "if query == 'appending':\n"
                f"    print('File: {store}, Lines: 205-207')\n"
                "    print('Remaining files not shown:')\n"
                "    for index in range(20): print(f'  src/review_cmd_foo_{index}.rs <1> <1>')\n"
                "    print('  src/review_cmd_dirty_tree.rs <1> <1>')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "appending", "review", "audit", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/review_cmd_dirty_tree.rs:125\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("appending", probe_queries)
        self.assertNotIn("append", probe_queries)
        self.assertNotIn("audit", probe_queries)

    def test_default_query_bm25_fast_path_joins_cache_key_and_skips_post_compress(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source_dir = repo / "src"
            source_dir.mkdir(parents=True)
            target = source_dir / "codex.py"
            preflight = source_dir / "preflight.py"
            target.write_text(
                "def _bounded_prompt_cache_key(): pass\n"
                "def _content_cache_key(): pass\n"
            )
            preflight.write_text("def should_compress_preflight(): pass\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                f"if query == 'cache_key':\n"
                f"    print('File: {target}, Lines: 1-1')\n"
                "    print('Remaining files not shown:')\n"
                f"    print('  {target.relative_to(repo)} <3> <46>')\n"
                f"elif query == 'post_compress': print('{preflight}:1')\n"
                "else: print('git-fixtures:1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where is cache key assembly after post-compress",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_queries = [
                json.loads(line)
                for line in (directory / "probe-trace.json").read_text().splitlines()
            ]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/codex.py:2\n")
        self.assertEqual(result.stderr, "")
        self.assertIn("cache_key", probe_queries)
        for forbidden in ("post_compress", "compress", "cache", "key"):
            self.assertNotIn(forbidden, probe_queries)

    def test_default_query_bm25_fast_path_requires_post_compress_compound(self) -> None:
        for include_compound in (True, False):
            with self.subTest(include_compound=include_compound), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                repo = directory / "repo"
                source_dir = repo / "src"
                source_dir.mkdir(parents=True)
                preflight = source_dir / "context_engine.py"
                compound = source_dir / "cache_key.py"
                preflight.write_text("# filler\n" * 331 + "def should_compress_preflight(): pass\n")
                compound.write_text("# filler\n" * 19 + "def assemble_post_compress_cache_key(): pass\n")
                env, _ = self.fake_environment(directory)
                env["PBI_TEST_COMPOUND"] = "1" if include_compound else "0"
                probe = directory / "probe"
                probe.write_text(
                    "#!/usr/bin/env python3\n"
                    "import json, os, sys\n"
                    "query = sys.argv[-1]\n"
                    "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: trace.write(json.dumps(query) + '\\n')\n"
                    f"if query == 'post_compress':\n    print('{preflight}:332')\n    if os.environ['PBI_TEST_COMPOUND'] == '1': print('{compound}:20')\n"
                    "else: print('git-fixtures:1')\n"
                )
                probe.chmod(0o755)
                result = self.run_pbi(
                    "post-compress", env=env, cwd=repo,
                    binary=self.fake_pbi(directory, probe),
                )
                probe_queries = [json.loads(line) for line in (directory / "probe-trace.json").read_text().splitlines()]
            self.assertIn("post_compress", probe_queries)
            self.assertNotIn("compress", probe_queries)
            self.assertNotIn("post-compress", probe_queries)
            if include_compound:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "src/cache_key.py:20\n")
                self.assertEqual(result.stderr, "")
            else:
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

if __name__ == "__main__":
    unittest.main(verbosity=2)
