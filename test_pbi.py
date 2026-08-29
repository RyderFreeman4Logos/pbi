#!/usr/bin/env python3
"""Focused hermetic checks for the pbi Probe Chat wrapper."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parent
PBI = ROOT / "pbi"
INSTALLER = ROOT / "install.sh"
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
        effective_cwd = cwd
        if effective_cwd is None and env is not None and env.get("HOME"):
            effective_cwd = Path(env["HOME"])
        return subprocess.run(
            [str(binary), *args],
            text=True,
            capture_output=True,
            check=False,
            env=env,
            cwd=effective_cwd,
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
        # Clear exactly the finite set of product-consumed host routing,
        # credential, and deadline variables so host knobs cannot change a
        # test's route or timing; each test then sets the values it requires.
        for name in (
            "CLIPROXY_API_KEY",
            "OPENAI_API_KEY",
            "LOCAL_ROUTER_API_KEY",
            "CLIPROXY_BASE_URL",
            "LOCAL_ROUTER_BASEURL",
            "LOCAL_MODEL",
            "LLM_MODEL",
            "FALLBACK_MODEL",
            "PBI_CONFIG_FILE",
            "XDG_CONFIG_HOME",
            "PBI_PLANNER_TIMEOUT_SECONDS",
            "PBI_CHAT_TIMEOUT_SECONDS",
            "REQUEST_TIMEOUT_MS",
            "MAX_OPERATION_TIMEOUT_MS",
        ):
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
        self.assertIn(
            "Search prints compact verified BM25 locations and never starts chat",
            help_result.stdout,
        )
        self.assertIn("--bm25 prints raw no-LLM Probe output", help_result.stdout)
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

    def test_positional_question_fails_closed_when_fast_path_has_no_candidates(self) -> None:
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
            probe_trace = directory / "probe-trace.json"
            self.assertTrue(probe_trace.exists(), "positional questions must retrieve code first")
            probe_calls = [json.loads(line) for line in probe_trace.read_text().splitlines()]
            self.assertTrue(probe_calls)
            self.assertTrue(all(call[1:7] == ["--timeout", "540", "--max-results", "4", "--max-tokens", "4000"] for call in probe_calls))
            self.assertFalse(trace.exists(), "a completed fast-path miss must skip planner and chat")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")

    def test_default_question_synthesizes_source_answer_instead_of_bm25_stamps(self) -> None:
        answer = "The check is implemented by exact_reuse_receipt in receipt.py:1."
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "receipt.py"
            source.write_text("def exact_reuse_receipt():\n    return True\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys\n"
                "open(os.environ['PBI_TEST_TRACE'], 'a').close()\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('guardian cutover readiness')\n"
                "    print('integrated guardian process')\n"
                "    print('hermetic readiness report')\n"
                "    print('deploy config isolation')\n"
                "    print('exact reuse receipt')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                f"    print({answer!r})\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "Why can the hermetic guardian cutover readiness test report that the integrated guardian main process changed?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            self.assertTrue(trace.exists(), "a default question must synthesize after BM25-only stamps")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{answer}\n")
        self.assertEqual(result.stderr, "")

    def test_why_question_answers_from_source_when_chat_omits_locations(self) -> None:
        # #120: BM25 hits exist, but chat returns location-less prose. A
        # fail-closed diagnostic is not a source-grounded answer.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "distractor.py").write_text("def unrelated():\n    return True\n")
            (repo / "LICENSE").write_text("Apache process changed terms\n" * 40)
            (repo / "Cargo.toml").write_text("[workspace]\nmembers = [\"guardian\"]\n")
            source = repo / "cutover-guardian.sh"
            source.write_text(
                "#!/bin/sh\n"
                'attestation_error="integrated guardian main process changed"\n'
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            distractor = repo / "distractor.py"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {distractor}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    print('guardian process')\n"
                "    print('cutover readiness')\n"
                "    print('deploy config')\n"
                "    print('hermetic report')\n"
                "    print('isolate tests')\n"
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('The guardian process changed because pids differ.')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "Why can the hermetic guardian cutover readiness test report that the integrated guardian main process changed?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(
            result.stdout.startswith("Located in "),
            result.stdout,
        )
        self.assertNotRegex(
            result.stdout,
            r"^Located in [^:]+:\d+(, [^:]+:\d+)*\.\n?\Z",
            result.stdout,
        )
        self.assertIn("cutover-guardian.sh", result.stdout)
        self.assertRegex(result.stdout, r"cutover-guardian\.sh:\d+")
        self.assertIn("attestation_error", result.stdout)
        self.assertIn("integrated guardian main process changed", result.stdout)
        self.assertNotIn("LICENSE:", result.stdout)
        self.assertNotIn("Cargo.toml:", result.stdout)
        self.assertEqual(result.stderr, "")
        self.assertNotIn("no source locations found", result.stderr)
        self.assertNotIn("timed out", result.stderr)

    def test_why_question_fails_closed_when_only_junk_stamps_remain(self) -> None:
        # Synthesis-class why/how questions must not succeed with leftover
        # compact BM25 stamps after junk (LICENSE/Cargo.toml/*.md) is filtered.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            license_path = repo / "LICENSE"
            license_path.write_text("Apache process changed terms\n" * 40)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {license_path}, Lines: 1-40')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Why can the hermetic guardian cutover readiness test report that the integrated guardian main process changed?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("LICENSE:1", result.stdout)
        self.assertNotIn("Located in", result.stdout)

    def test_multi_target_where_recovers_bm25_candidates_after_chat_timeout(self) -> None:
        # A multi-target answer must not turn one unrelated BM25 hit into success.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            tests = repo / "tests"
            tests.mkdir(parents=True)
            (repo / "distractor.py").write_text("def unrelated():\n    return True\n")
            receipt = tests / "quality-gate-receipt-tests.sh"
            receipt.write_text("run_exact_reuse() {\n  echo exact-reuse\n}\n")
            isolation = tests / "quality-gate-isolation-tests.sh"
            isolation.write_text("ambient-inputs)\n  isolate_ambient_inputs\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            distractor = repo / "distractor.py"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {distractor}, Lines: 1-2')\n"
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
                "Where are the quality-gate exact-reuse receipt contract test, the shared receipt helper it exercises, and the ambient-inputs isolation implementation?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
            elapsed = time.monotonic() - started
            self.assertTrue(trace.exists(), "Probe Chat must start before timing out")
        self.assertEqual(result.returncode, 1, (result.stdout, result.stderr))
        self.assertEqual(result.stdout, "")
        self.assertIn(
            result.stderr,
            {
                "pbi: source answer lacks requested semantic evidence\n",
                "pbi: model returned only BM25 location stamps; no source answer\n",
                "pbi: no source locations found\n",
            },
        )
        self.assertLess(elapsed, 6)

    def test_where_are_question_quotes_bm25_source_instead_of_stamps(self) -> None:
        # #123: a natural-language where-are question whose BM25 hits include
        # real source must quote a cited line. Stamp-only fail-closed is not
        # an answer, even when distinctive-token rg cannot see the hit.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "request_cache.py"
            source.write_text(
                "def store_request_prefix(payload):\n"
                "    return hash((prefix, payload))\n"
            )
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
                "print('request_cache.py:1')\n"
                "print('request_cache.py:1')\n"
                "print('request_cache.py:1')\n"
                "print('request_cache.py:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are provider request prefixes or API request bodies stored for cache identity",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(result.stdout.startswith("Located in "), result.stdout)
        self.assertNotRegex(
            result.stdout,
            r"^Located in [^:]+:\d+(, [^:]+:\d+)*\.\n?\Z",
            result.stdout,
        )
        self.assertIn("request_cache.py", result.stdout)
        self.assertRegex(result.stdout, r"request_cache\.py:\d+")
        self.assertIn("store_request_prefix", result.stdout)
        self.assertEqual(result.stderr, "")
        self.assertNotIn("location stamps", result.stderr)
        self.assertNotIn("no source locations found", result.stderr)

    def test_where_are_question_rejects_unrelated_bm25_provider_hit(self) -> None:
        # #123: leftover stopword survivors like provider/identity must not
        # turn an unrelated BM25 hit into a source answer.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            unrelated = repo / "batch_runner.py"
            unrelated.write_text(
                "# bearer provider returned by agent.azure_identity_adapter\n"
                "# token provider in the worker process (azure-identity caches\n"
                "# Fail closed if a job's stored provider/base_url pair would leak\n"
                "# provider's stored key is never paired with an off-host base_url\n"
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {unrelated}, Lines: 1-4')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "print('batch_runner.py:1')\n"
                "print('batch_runner.py:1')\n"
                "print('batch_runner.py:1')\n"
                "print('batch_runner.py:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are provider request prefixes or API request bodies stored for cache identity",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("batch_runner.py", result.stdout)
        self.assertNotIn("azure-identity", result.stdout)
        self.assertNotIn("azure_identity", result.stdout)
        self.assertIn("location stamps", result.stderr)

    def test_where_are_question_rejects_path_only_overlap_on_unrelated_line(self) -> None:
        # #123: a filename matching the query phrase must not validate an
        # unrelated quoted line (shebang / comment).
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source = repo / "src" / "cache_identity.py"
            source.parent.mkdir(parents=True)
            source.write_text("#!/usr/bin/env bash\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "print('src/cache_identity.py:1')\n"
                "print('src/cache_identity.py:1')\n"
                "print('src/cache_identity.py:1')\n"
                "print('src/cache_identity.py:1')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are provider request prefixes or API request bodies stored for cache identity",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("cache_identity.py", result.stdout)
        self.assertNotIn("#!/usr/bin/env bash", result.stdout)
        self.assertIn("location stamps", result.stderr)

    def test_where_does_question_emits_from_relevant_bm25_hits(self) -> None:
        # #126: relevant quoted-line BM25 hits must become a source answer.
        # Falling through to planner/chat just to time out empty is the fail.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "review_bypass.py"
            source.write_text(
                "def accept_native_review_bypass_evidence(payload):\n"
                "    return payload\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where does native review bypass evidence get accepted",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("review_bypass.py", result.stdout)
        self.assertIn("accept_native_review_bypass_evidence", result.stdout)
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "in-hand quoted-line hits must not start planner")
        self.assertNotIn("timed out", result.stderr)
        self.assertLess(elapsed, 6)

    def test_where_does_question_emits_hyphenated_identifier_overlap(self) -> None:
        # #126 live: "native review bypass" must match native_bypass_reason /
        # native-review-bypass.sh. A line without the leftover word "evidence"
        # is still a relevant quoted hit.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "workflow.toml"
            source.write_text(
                'if native_bypass_reason="$(bash native-review-bypass.sh)"; then\n'
                "    return 0\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where does pr-bot Step 10b accept native review bypass evidence",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("workflow.toml", result.stdout)
        self.assertIn("native-review-bypass.sh", result.stdout)
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "in-hand quoted-line hits must not start planner")
        self.assertLess(elapsed, 6)

    def test_which_test_module_rejects_type_declarations_and_import_lists(self) -> None:
        # #130: declarations, imports, and non-test source do not answer coverage questions.
        for path, line in (
            ("migration_framework.rs", "pub struct WorkflowRun {"),
            ("migration_framework.rs", "pub type WorkflowRun = u64;"),
            ("migration_framework.rs", "    WorkflowRun,"),
            ("sdk/workflow_run.rs", "WorkflowRun workflow envelope identity does not match the payload"),
        ):
            with self.subTest(path=path, line=line), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                repo = directory / "repo"
                repo.mkdir()
                source = repo / path
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("\n" * 138 + f"{line}\n")
                env, trace = self.fake_environment(directory)
                probe = directory / "probe"
                probe.write_text(
                    "#!/usr/bin/env python3\n"
                    f"print('File: {source}, Lines: 139-139')\n"
                )
                probe.chmod(0o755)
                result = self.run_pbi(
                    "Which test module covers WorkflowRun wire serialization",
                    env=env,
                    cwd=repo,
                    binary=self.fake_pbi(directory, probe),
                    timeout=8,
                )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertFalse(trace.exists() and "timed out" in result.stderr)

    def test_which_test_module_emits_test_module_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            source = repo / "sdk_tests" / "workflow_wire.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn live_workflow_wire_valid_envelope_round_trips() {\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Which test module covers WorkflowRun wire serialization",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("sdk_tests/workflow_wire.rs", result.stdout)
        self.assertIn("live_workflow_wire_valid_envelope_round_trips", result.stdout)
        self.assertFalse(trace.exists())

    def test_classify_question_rejects_lone_type_declaration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "run.rs"
            source.write_text("\n" * 129 + "pub struct WorkflowRun {\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 130-130')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Classify every WorkflowRun occurrence by construction, publication, validation, or taxonomy",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")

    def test_find_question_emits_from_named_path_quoted_line(self) -> None:
        # #129: a Find/path question with a relevant quoted line in the named
        # file must emit that line, not fall through to a planner timeout.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            named = repo / "patterns" / "pr-bot" / "scripts" / "csa"
            named.mkdir(parents=True)
            source = named / "session-wait-until-done.sh"
            source.write_text(
                "#!/usr/bin/env bash\n"
                'echo "usage: session-wait-until-done.sh <session-id>" >&2\n'
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "Find patterns/pr-bot/scripts/csa/session-wait-until-done.sh, all direct callers, and its regression tests.",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("session-wait-until-done.sh", result.stdout)
        self.assertIn("usage:", result.stdout)
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "in-hand quoted-line hits must not start planner")
        self.assertLess(elapsed, 6)

    def test_where_does_question_late_bm25_recovery_falls_through(self) -> None:
        # #126: 8s bounds BM25 recovery reads only. A late recovery on a
        # non-synthesis question must fall through to planner/chat instead of
        # aborting the whole command with the stamp diagnostic.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "review_bypass.py"
            source.write_text(
                "def accept_native_review_bypass_evidence(payload):\n"
                "    return payload\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            real_sed = shutil.which("sed")
            self.assertIsNotNone(real_sed)
            sed_count = directory / "sed-count"
            fake_sed = directory / "sed"
            fake_sed.write_text(
                "#!/usr/bin/env bash\n"
                f"count_file={sed_count}\n"
                "n=0\n"
                '[[ -f "$count_file" ]] && n=$(<"$count_file")\n'
                "n=$((n + 1))\n"
                'printf "%s\\n" "$n" > "$count_file"\n'
                '[[ "$n" -eq 1 ]] && sleep 2\n'
                f'exec {real_sed} "$@"\n'
            )
            fake_sed.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(
                binary.read_text().replace(
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="1"',
                )
            )
            binary.chmod(0o755)
            result = self.run_pbi(
                "where does native review bypass evidence get accepted",
                env=env,
                cwd=repo,
                binary=binary,
                timeout=4,
            )
            planner_started = trace.exists()
        recovered = (
            result.returncode == 0
            and "accept_native_review_bypass_evidence" in result.stdout
            and "review_bypass.py" in result.stdout
        )
        self.assertTrue(
            recovered or planner_started,
            f"rc={result.returncode} stdout={result.stdout!r} stderr={result.stderr!r}",
        )
        self.assertNotIn("location stamps", result.stderr)
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")

    def test_where_are_question_late_bm25_recovery_emits_or_falls_through(self) -> None:
        # #123: 8s bounds BM25 recovery reads. A relevant recovered line is
        # still emitted, or planner/chat may still run. Stamp-only whole-command
        # abort is the over-fix.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "request_cache.py"
            source.write_text(
                "def store_request_prefix(payload):\n"
                "    return hash((prefix, payload))\n"
            )
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            real_sed = shutil.which("sed")
            self.assertIsNotNone(real_sed)
            sed_count = directory / "sed-count"
            fake_sed = directory / "sed"
            fake_sed.write_text(
                "#!/usr/bin/env bash\n"
                f"count_file={sed_count}\n"
                "n=0\n"
                '[[ -f "$count_file" ]] && n=$(<"$count_file")\n'
                "n=$((n + 1))\n"
                'printf "%s\\n" "$n" > "$count_file"\n'
                '[[ "$n" -eq 1 ]] && sleep 2\n'
                f'exec {real_sed} "$@"\n'
            )
            fake_sed.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(
                binary.read_text().replace(
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="1"',
                )
            )
            binary.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where are provider request prefixes or API request bodies stored for cache identity",
                env=env,
                cwd=repo,
                binary=binary,
                timeout=4,
            )
            elapsed = time.monotonic() - started
        recovered = (
            result.returncode == 0
            and "store_request_prefix" in result.stdout
            and "request_cache.py" in result.stdout
        )
        self.assertTrue(
            recovered or trace.exists(),
            f"rc={result.returncode} stdout={result.stdout!r} stderr={result.stderr!r}",
        )
        self.assertNotIn("location stamps", result.stderr)
        self.assertLess(elapsed, 2.4)

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
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
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

    def test_default_query_named_readme_prefers_product_claim_over_caption_test(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            docs = repo / "docs"
            docs.mkdir(parents=True)
            readme = repo / "README.md"
            mvp = docs / "mvp.md"
            readme_lines = [
                "# Product claims",
                "OCR output and vision captions are not citable source Evidence.",
            ]
            readme.write_text("\n".join(readme_lines) + "\n")
            mvp.write_text("# MVP\nGenerated captions are not Evidence.\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "query = sys.argv[-1]\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as trace: "
                "    trace.write(json.dumps(query) + '\\n')\n"
                "if query == 'caption': print('src/vision_caption.rs:291')\n"
                "else: print('src/vision_caption.rs:291')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Locate current safety, hallucination, caption, OCR, and evidence product claims in README and docs/mvp.md; report exact files and headings.",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
            probe_trace = directory / "probe-trace.json"
            probe_queries = [
                json.loads(line) for line in probe_trace.read_text().splitlines()
            ] if probe_trace.exists() else []
            claim_line = readme_lines.index("OCR output and vision captions are not citable source Evidence.") + 1
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"README.md:{claim_line}\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(any(query in {
            "caption", "hallucination", "headings", "product", "report", "evidence", "safety"
        } for query in probe_queries))

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

    def test_default_query_post_bm25_recovery_falls_through_after_deadline(self) -> None:
        # #126: 8s bounds BM25 recovery reads only. A late named-symbol rg
        # that cannot recover from the BM25 hit must fall through.
        symbol = "TargetSymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "unrelated.py").write_text("pass\n")
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if '--dry-run' in sys.argv:\n"
                "    print('File: unrelated.py, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            slow_rg = directory / "rg"
            slow_rg.write_text("#!/usr/bin/env bash\nsleep 2\nprintf '%s\\n' './target.py'\n")
            slow_rg.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(
                binary.read_text().replace(
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="1"',
                )
            )
            binary.chmod(0o755)
            result = self.run_pbi(
                "where", "is", symbol, env=env, cwd=repo, binary=binary, timeout=4
            )
            planner_started = trace.exists()
        self.assertTrue(planner_started, "late BM25 recovery must fall through to planner/chat")
        self.assertNotIn("location stamps", result.stderr)

    def test_default_query_term_resistant_initial_probe_fits_absolute_deadline(self) -> None:
        # #118 absolute deadline: the initial BM25 probe search plus its
        # same-group TERM-ignoring child must be TERM/KILL/reaped inside the
        # configured budget, leave zero matching identities before emergency
        # cleanup, and fail closed without planner/chat.
        def current_start_time(pid: int) -> str | None:
            try:
                return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
            except (FileNotFoundError, IndexError, ProcessLookupError):
                return None

        symbol = "TargetSymbol"
        identities: dict[str, dict[str, int | str]] = {}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "unrelated.py").write_text("pass\n")
            env, trace = self.fake_environment(directory)
            identity_file = directory / "probe-identities.json"
            child_identity_file = directory / "probe-child.json"
            env["PBI_TEST_PROBE_IDENTITIES"] = str(identity_file)
            env["PBI_TEST_PROBE_CHILD_IDENTITY"] = str(child_identity_file)
            probe = directory / "probe"
            child_code = (
                "import json, os, pathlib, signal, sys, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "start = pathlib.Path(f'/proc/{os.getpid()}/stat').read_text().rsplit(')', 1)[1].split()[19]\n"
                "pathlib.Path(sys.argv[1]).write_text(json.dumps({'pid': os.getpid(), 'start_time': start, 'pgid': os.getpgrp()}))\n"
                "while True: time.sleep(.1)\n"
            )
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, pathlib, signal, subprocess, sys, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                f"child_code = {child_code!r}\n"
                "child_path = pathlib.Path(os.environ['PBI_TEST_PROBE_CHILD_IDENTITY'])\n"
                "child = subprocess.Popen([sys.executable, '-c', child_code, str(child_path)])\n"
                "deadline = time.monotonic() + .5\n"
                "while not child_path.exists() and time.monotonic() < deadline: time.sleep(.005)\n"
                "child_identity = json.loads(child_path.read_text())\n"
                "start = pathlib.Path(f'/proc/{os.getpid()}/stat').read_text().rsplit(')', 1)[1].split()[19]\n"
                "identities = {'parent': {'pid': os.getpid(), 'start_time': start, 'pgid': os.getpgrp()}, 'child': child_identity}\n"
                "pathlib.Path(os.environ['PBI_TEST_PROBE_IDENTITIES']).write_text(json.dumps(identities))\n"
                "while True: time.sleep(.1)\n"
            )
            probe.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(binary.read_text().replace(
                'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="3"',
            ))
            binary.chmod(0o755)
            try:
                started = time.monotonic()
                result = self.run_pbi("where", "is", symbol, env=env, cwd=repo, binary=binary, timeout=6)
                elapsed = time.monotonic() - started
                identities = json.loads(identity_file.read_text())
                self.assertEqual(identities["parent"]["pgid"], identities["child"]["pgid"])
                survivors = {
                    name: identity
                    for name, identity in identities.items()
                    if current_start_time(int(identity["pid"])) == identity["start_time"]
                }
                self.assertFalse(
                    survivors,
                    f"TERM/KILL/reap must complete inside the deadline; matching identities: {survivors}",
                )
            finally:
                for identity in identities.values():
                    pid = int(identity["pid"])
                    try:
                        pidfd = os.pidfd_open(pid)
                    except (AttributeError, ProcessLookupError):
                        continue
                    try:
                        if current_start_time(pid) == identity["start_time"]:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                    finally:
                        os.close(pidfd)
                cleanup_deadline = time.monotonic() + .5
                while time.monotonic() < cleanup_deadline and any(
                    current_start_time(int(identity["pid"])) == identity["start_time"]
                    for identity in identities.values()
                ):
                    time.sleep(.01)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertLess(elapsed, 3.2, "the initial probe TERM/KILL/reap must fit the absolute budget")
        self.assertFalse(trace.exists(), "a deadline miss must not start planner/chat")

    def test_default_query_named_readme_without_claim_fails_closed(self) -> None:
        # #118: a completed named-file fast-path miss (README with no
        # recognized claim) must fail closed without ever starting
        # planner/chat or invoking the probe.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "README.md").write_text("# README\nSome product notes.\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            self.record_probe_argv(probe)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where", "is", "README", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            probe_invoked = (directory / "probe-trace.json").exists()
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertFalse(probe_invoked, "a README miss must not invoke the probe")
        self.assertFalse(trace.exists(), "a README miss must never start planner/chat")

    def test_default_query_term_ignoring_rg_is_killed_inside_deadline(self) -> None:
        def current_start_time(pid: int) -> str | None:
            try:
                return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
            except (FileNotFoundError, IndexError, ProcessLookupError):
                return None

        symbol = "TargetSymbol"
        identities: dict[str, dict[str, int | str]] = {}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "unrelated.py").write_text("pass\n")
            env, trace = self.fake_environment(directory)
            identity_file = directory / "rg-identities.json"
            child_identity_file = directory / "rg-child.json"
            env["PBI_TEST_RG_IDENTITIES"] = str(identity_file)
            env["PBI_TEST_RG_CHILD_IDENTITY"] = str(child_identity_file)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if '--dry-run' in sys.argv: print('File: unrelated.py, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            child_code = (
                "import json, os, pathlib, signal, sys, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "start = pathlib.Path(f'/proc/{os.getpid()}/stat').read_text().rsplit(')', 1)[1].split()[19]\n"
                "pathlib.Path(sys.argv[1]).write_text(json.dumps({'pid': os.getpid(), 'start_time': start, 'pgid': os.getpgrp()}))\n"
                "while True: time.sleep(.1)\n"
            )
            rg = directory / "rg"
            rg.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, pathlib, signal, subprocess, sys, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                f"child_code = {child_code!r}\n"
                "child_path = pathlib.Path(os.environ['PBI_TEST_RG_CHILD_IDENTITY'])\n"
                "child = subprocess.Popen([sys.executable, '-c', child_code, str(child_path)])\n"
                "deadline = time.monotonic() + .5\n"
                "while not child_path.exists() and time.monotonic() < deadline: time.sleep(.005)\n"
                "child_identity = json.loads(child_path.read_text())\n"
                "start = pathlib.Path(f'/proc/{os.getpid()}/stat').read_text().rsplit(')', 1)[1].split()[19]\n"
                "identities = {'parent': {'pid': os.getpid(), 'start_time': start, 'pgid': os.getpgrp()}, 'child': child_identity}\n"
                "pathlib.Path(os.environ['PBI_TEST_RG_IDENTITIES']).write_text(json.dumps(identities))\n"
                "while True: time.sleep(.1)\n"
            )
            rg.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(binary.read_text().replace(
                'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="1"',
            ))
            binary.chmod(0o755)
            try:
                started = time.monotonic()
                result = self.run_pbi("where", "is", symbol, env=env, cwd=repo, binary=binary, timeout=3)
                elapsed = time.monotonic() - started
                identities = json.loads(identity_file.read_text())
                self.assertEqual(identities["parent"]["pgid"], identities["child"]["pgid"])
                survivors = {
                    name: identity
                    for name, identity in identities.items()
                    if current_start_time(int(identity["pid"])) == identity["start_time"]
                }
                self.assertFalse(survivors, f"deadline cleanup left matching process identities: {survivors}")
            finally:
                for identity in identities.values():
                    pid = int(identity["pid"])
                    try:
                        pidfd = os.pidfd_open(pid)
                    except (AttributeError, ProcessLookupError):
                        continue
                    try:
                        if current_start_time(pid) == identity["start_time"]:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                    finally:
                        os.close(pidfd)
                cleanup_deadline = time.monotonic() + .5
                while time.monotonic() < cleanup_deadline and any(
                    current_start_time(int(identity["pid"])) == identity["start_time"]
                    for identity in identities.values()
                ):
                    time.sleep(.01)
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(elapsed, 1.8)
        self.assertNotIn("location stamps", result.stderr)

    def test_default_query_named_recovery_succeeds_before_absolute_deadline(self) -> None:
        symbol = "TargetSymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "target.py"
            source.write_text(f"class {symbol}: pass\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                f"if '--dry-run' in sys.argv: print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fast_rg = directory / "rg"
            fast_rg.write_text("#!/usr/bin/env bash\nprintf '%s\\n' './target.py'\n")
            fast_rg.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            binary.write_text(
                binary.read_text().replace(
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="8"',
                    'readonly DEFAULT_FAST_PATH_SEARCH_TIMEOUT_SECONDS="1"',
                )
            )
            binary.chmod(0o755)
            result = self.run_pbi(
                "where", "is", symbol, env=env, cwd=repo, binary=binary, timeout=4
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "target.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "successful recovery must skip planner/chat")

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
            planner_started = (directory / "trace.json").exists()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("location stamps", result.stderr)
        self.assertTrue(planner_started, "unrelated BM25 leftovers must fall through")
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
            self.assertFalse(trace.exists(), "planner/chat must not run after a completed fast-path miss")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "src/api/mcp.rs:1\nsrc/api/mcp.rs:2\n")
        self.assertEqual(result.stderr, "")

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
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")

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
        self.assertEqual(result.stderr, "pbi: no source locations found\n")

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
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
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
            self.assertFalse(trace.exists(), "planner/chat must not run after a completed fast-path miss")
        self.assertEqual(result.returncode, 1)
        self.assertLess(elapsed, 3)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertNotIn("LICENSE:1", result.stdout + result.stderr)

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
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertNotIn("SEARCH_SENTINEL", result.stdout + result.stderr)
        self.assertNotIn("PLANNER_SENTINEL", result.stdout + result.stderr)

    def test_default_query_lone_non_one_stamp_is_not_success(self) -> None:
        # #126/#130: a lone path:line stamp, including non-1 lines, is never
        # rc=0 stdout. Quote a relevant line or fail closed.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "run.py"
            source.write_text("\n" * 129 + "class WorkflowRun: pass\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 130-130')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Which test module covers WorkflowRun wire serialization",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.stdout.strip(), "run.py:130")
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")
        if result.returncode == 0:
            self.assertIn("run.py", result.stdout)
            self.assertIn("WorkflowRun", result.stdout)
            self.assertIn("class WorkflowRun", result.stdout)
            self.assertEqual(result.stderr, "")
        else:
            self.assertEqual(result.stdout, "")
            self.assertIn("location stamps", result.stderr)

    def test_find_question_lone_stamp_is_not_success(self) -> None:
        # #126/#129: a Find/path question must not succeed with a lone
        # file:line stamp. Quote a relevant line or fail closed / fall through.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            unrelated = repo / "review_cmd_prose_findings.rs"
            unrelated.write_text("\n" * 249 + "fn session_wait_until_done() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {unrelated}, Lines: 250-250')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Find patterns/pr-bot/scripts/csa/session-wait-until-done.sh, all direct callers, and its regression tests.",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
            )
        self.assertNotEqual(result.stdout.strip(), "review_cmd_prose_findings.rs:250")
        self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")

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
            self.assertIn(answer, result.stdout)
            self.assertEqual(result.stderr, "")

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
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
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
            self.assertFalse(trace.exists(), "a completed fast-path miss must skip planner and chat")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")

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
            self.assertFalse(trace.exists(), "named-symbol search miss must skip chat")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
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

    def test_search_hides_mocked_bert_fallback_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            (directory / "reference.py").write_text("print(SessionDB)\n")
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"BERT reranker 'ms-marco-minilm-l6' is not available.\"\n"
                "printf '%s\\n' 'Falling back to BM25 ranking...'\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("search", "SessionDB", env=env, binary=self.fake_pbi(directory, probe))
            self.assertFalse(trace.exists(), "an empty BM25 result must skip chat")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertNotIn("BERT reranker", result.stdout)
        self.assertNotIn("Falling back to BM25", result.stdout)

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

    def test_search_compact_fixture_records_probe_input_and_zero_chat(self) -> None:
        # #118: the default explicit search is a compact verified BM25
        # localization; the fixture proves the exact one Probe search input,
        # zero chat invocations, and the exact compact output.
        symbol = "rest_response_prefers_created_ids_when_both_fields_exist"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text(f"def {symbol}():\n    return True\n")
            env, trace = self.fake_environment(directory)
            probe_trace = directory / "probe-trace.json"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as f:\n"
                "    f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                f"print(f'File: {repo}/real.py, Lines: 1-2')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", f"Locate {symbol}", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            probe_invocations = [json.loads(line) for line in probe_trace.read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "no chat invocation is allowed for default search")
        self.assertEqual(
            probe_invocations,
            [
                [
                    "search", "--timeout", "540", "--max-results", "8", "--ignore", "drafts",
                    "--reranker", "bm25", "--format", "plain", "--dry-run", "--",
                    f"Locate {symbol}",
                ]
            ],
        )

    def test_search_bm25_fixture_records_raw_probe_output_and_zero_chat(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text("def TargetSymbol():\n    return True\n")
            env, trace = self.fake_environment(directory)
            probe_trace = directory / "probe-trace.json"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport json, os, sys\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'a') as f:\n"
                "    f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "print('File: real.py, Lines: 1-1')\n"
                "print('raw probe line 2')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", "--bm25", "TargetSymbol", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            probe_invocations = [json.loads(line) for line in probe_trace.read_text().splitlines()]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(trace.exists(), "raw --bm25 output must not invoke chat")
        self.assertEqual(result.stdout, "File: real.py, Lines: 1-1\nraw probe line 2\n")
        self.assertEqual(result.stderr, "")
        self.assertEqual(
            probe_invocations,
            [["search", "--reranker", "bm25", "--timeout", "540", "--max-results", "8", "--", "TargetSymbol"]],
        )

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
            (directory / "reference.py").write_text("use(HERMES_TUI_RPC_TIMEOUT_MS)\n")
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
        self.assertTrue(
            "location stamps" in result.stderr or "no source locations found" in result.stderr,
            result.stderr,
        )

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
            # candidate recovery must finish without invoking Probe Chat.
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

    def test_search_generic_probe_failure_recovers_candidates_without_chat(self) -> None:
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

    def test_search_generic_probe_failure_without_candidates_fails_closed_before_chat(self) -> None:
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
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(
            result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n"
        )

    def test_search_api_error_recovers_candidate_without_chat(self) -> None:
        # #22: an API-error payload must not hide an already-retrieved location
        # for a natural-language query; candidate recovery returns it directly.
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
            env, trace = self.fake_environment(directory)
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
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            started = time.monotonic()
            result = self.run_pbi(
                "search", "Locate", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertFalse(trace.exists(), "a named-symbol miss must skip Probe Chat")
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
            env, trace = self.fake_environment(directory)
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
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            started = time.monotonic()
            result = self.run_pbi(
                "search", "Locate", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: model returned only BM25 location stamps; no source answer\n")
        self.assertFalse(trace.exists(), "a named-symbol miss must skip Probe Chat")
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

    def test_search_prefers_named_symbol_definition_over_import_mention(self) -> None:
        symbol = "TargetSymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "mention.py").write_text(f"from pkg import {symbol}\n")
            (repo / "pkg.py").write_text(f"class {symbol}:\n    pass\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            rg = directory / "rg"
            rg.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$1\" == \"-l\" ]]; then\n"
                "    printf '%s\n' './mention.py' './pkg.py'\n"
                "    exit 0\n"
                "fi\n"
                "exec /usr/bin/rg \"$@\"\n"
            )
            rg.chmod(0o755)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pkg.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "named-symbol recovery must skip Probe Chat")
        self.assertLess(elapsed, 5)

    def test_search_recovers_occurrence_outside_unrelated_bm25_candidate(self) -> None:
        symbol = "UseOnlySymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            unrelated = repo / "unrelated.py"
            unrelated.write_text("pass\n")
            (repo / "real.py").write_text(f"consume({symbol})\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(f"#!/usr/bin/env python3\nprint('File: {unrelated}, Lines: 1-1')\n")
            probe.chmod(0o755)
            result = self.run_pbi("search", symbol, env=env, cwd=repo,
                                  binary=self.fake_pbi(directory, probe), timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "real.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists())

    def test_search_prefers_named_symbol_definition_over_multiline_import_mention(self) -> None:
        symbol = "TargetSymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "mention.py").write_text(
                "from pkg import (\n"
                f"    {symbol},\n"
                ")\n"
            )
            (repo / "pkg.py").write_text(f"class {symbol}:\n    pass\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            rg = directory / "rg"
            rg.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$1\" == \"-l\" ]]; then\n"
                "    printf '%s\n' './mention.py' './pkg.py'\n"
                "    exit 0\n"
                "fi\n"
                "exec /usr/bin/rg \"$@\"\n"
            )
            rg.chmod(0o755)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pkg.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "named-symbol recovery must skip Probe Chat")

    def test_named_symbol_qualified_variant_is_valid_outside_enum(self) -> None:
        symbol = "Variant"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.rs"
            source.write_text("fn use_variant() { Type::Variant; }\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(f"#!/usr/bin/env python3\nprint('File: {source}, Lines: 1-1')\n")
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "project.rs:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists())

    def test_named_symbol_lone_use_after_enum_is_not_a_definition(self) -> None:
        symbol = "Ghost"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.rs"
            source.write_text("enum Real {\n    Actual,\n}\nfn use_it() {\n    Ghost => 1;\n}\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                f"if '--dry-run' in sys.argv: print('File: {source}, Lines: 5-5')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where", "is", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("project.rs:5", result.stdout + result.stderr)
        self.assertFalse(trace.exists(), "an unrelated lone symbol must not start planner/chat")

    def test_named_symbol_char_literal_brace_does_not_extend_enum(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.rs"
            source.write_text("enum Real { Actual = '{' as u8, }\nfn use_it() {\n    Ghost => 1;\n}\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport sys\n"
                f"if '--dry-run' in sys.argv: print('File: {source}, Lines: 3-3')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("where", "is", "Ghost", env=env, cwd=repo,
                                  binary=self.fake_pbi(directory, probe), timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("project.rs:3", result.stdout + result.stderr)
        self.assertFalse(trace.exists())

    def test_named_symbol_ts_single_quoted_enum_value_does_not_extend_enum(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.ts"
            source.write_text("enum Real { Actual = 'value{' }\nfn use_it() {\n    Ghost => 1;\n}\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport sys\n"
                f"if '--dry-run' in sys.argv: print('File: {source}, Lines: 3-3')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("where", "is", "Ghost", env=env, cwd=repo,
                                  binary=self.fake_pbi(directory, probe), timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("project.ts:3", result.stdout + result.stderr)
        self.assertFalse(trace.exists())

    def test_named_symbol_double_quoted_url_enum_value_keeps_string_content(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.ts"
            source.write_text('enum Real { Actual = "http://x" }\nfn use_it() {\n    Ghost => 1;\n}\n')
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport sys\n"
                f"if '--dry-run' in sys.argv: print('File: {source}, Lines: 3-3')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi("where", "is", "Ghost", env=env, cwd=repo,
                                  binary=self.fake_pbi(directory, probe), timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("project.ts:3", result.stdout + result.stderr)
        self.assertFalse(trace.exists())

    def test_default_query_rust_lifetimes_preserve_enum_structure(self) -> None:
        expected = {
            "DatabaseBusy": (2, ("where", "is", "DatabaseBusy")),
            "RealLoneVariant": (6, ("where", "is", "RealLoneVariant")),
            "QualifiedVariant": (9, ("where", "is", "QualifiedVariant")),
        }
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "project.rs"
            source.write_text(
                "enum X<'a> {\n"
                "    DatabaseBusy(&'a str),\n"
                r"    Quote = '\'' as u8," "\n"
                r"    Backslash = '\\' as u8," "\n"
                "    Brace = '{' as u8,\n"
                "    RealLoneVariant,\n"
                "}\n"
                "fn use_it<'a>(input: &'a str) {\n"
                "    Type::QualifiedVariant;\n"
                "    'label: loop { break 'label; }\n"
                "    Ghost => 1;\n"
                "}\n"
            )
            unrelated = repo / "unrelated.rs"
            unrelated.write_text("fn unrelated() {}\n")
            env, trace = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport sys\n"
                f"if '--dry-run' in sys.argv: print('File: {unrelated}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            binary = self.fake_pbi(directory, probe)
            for symbol, (line, query) in expected.items():
                with self.subTest(symbol=symbol):
                    result = self.run_pbi(*query, env=env, cwd=repo, binary=binary, timeout=5)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, f"project.rs:{line}\n")
                    self.assertEqual(result.stderr, "")
            ghost = self.run_pbi("where", "is", "Ghost", env=env, cwd=repo, binary=binary, timeout=5)
            self.assertFalse(trace.exists(), "definition recovery must not start planner/chat")
        self.assertNotEqual(ghost.returncode, 0)
        self.assertNotIn("project.rs:11", ghost.stdout + ghost.stderr)

    def test_named_symbol_variant_definition_beats_mention(self) -> None:
        symbol = "DatabaseBusy"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "mention.rs").write_text(
                "fn is_not_git_repository_stderr() {\n"
                f"    let message = \"{symbol}\";\n"
                "}\n"
            )
            (repo / "project.rs").write_text("enum X {\n    DatabaseBusy,\n}\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            rg = directory / "rg"
            rg.write_text(
                "#!/usr/bin/env bash\n"
                "if [[ \"$1\" == \"-l\" ]]; then\n"
                "    printf \"%s\\n\" \"./mention.rs\" \"./project.rs\"\n"
                "    exit 0\n"
                "fi\n"
                "exec /usr/bin/rg \"$@\"\n"
            )
            rg.chmod(0o755)
            for args in (("search", symbol), ("where", "is", symbol)):
                result = self.run_pbi(
                    *args, env=env, cwd=repo,
                    binary=self.fake_pbi(directory, directory / "probe"), timeout=5,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "project.rs:2\n")
                self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "enum-variant recovery must skip Probe Chat")

    def test_default_positional_recovers_enum_variant_before_bm25_stamp(self) -> None:
        symbol = "DatabaseBusy"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            retry_source = repo / "sqlite_retry.rs"
            retry_source.write_text("\n".join(["// filler"] * 22 + [f"    return {symbol};", ""]))
            variant_source = repo / "project.rs"
            variant_source.write_text("enum X {\n    DatabaseBusy,\n}\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "if '--dry-run' in sys.argv:\n"
                f"    print('File: {retry_source}, Lines: 23-23')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "where", "is", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "project.rs:2\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "named-symbol recovery must skip Probe Chat")

    def test_default_positional_recovers_named_symbol_definition(self) -> None:
        symbol = "TargetSymbol"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "mention.py").write_text(f"from pkg import {symbol}\n")
            (repo / "pkg.py").write_text(f"class {symbol}:\n    pass\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\nexit 0\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "where", "is", symbol, env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pkg.py:1\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "named-symbol recovery must skip planner and chat")
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

    def test_search_no_named_symbol_skips_hanging_chat_and_emits_bm25_location(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "README.md"
            source.write_text("# README\nproduct claim\nEvidence\ndocs/mvp\nclosing\n")
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {source}, Lines: 1-5')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search",
                "hallucination",
                "caption",
                "OCR",
                "Evidence",
                "README",
                "docs/mvp",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=5,
            )
            elapsed = time.monotonic() - started
            self.assertFalse(trace.exists(), "a no-symbol BM25 answer must skip hanging Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "README.md:3\n")
        self.assertEqual(result.stderr, "")
        self.assertLess(elapsed, 5)

    def test_search_bm25_emit_prefers_named_readme_over_caption_helper(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            readme = repo / "README.md"
            caption = repo / "crates" / "verbatim-core" / "src" / "vision_caption.rs"
            caption.parent.mkdir(parents=True)
            readme.write_text(
                "# Product claims\n"
                "This fixture has a source claim.\n"
                "Product Evidence claims belong in README.\n"
            )
            caption.write_text(
                'pub const VISION_CAPTION_PROMPT_VERSION: &str = "1";\n'
            )
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "touch \"$PBI_TEST_TRACE\"\n"
                "sleep 30\n"
            )
            fake_chat.chmod(0o755)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {caption}, Lines: 1-9')\n"
            )
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search",
                "hallucination",
                "caption",
                "OCR",
                "Evidence",
                "README",
                "docs/mvp",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=10,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "README.md:3\n")
        self.assertEqual(result.stderr, "")
        self.assertFalse(trace.exists(), "named-file recovery must skip Probe Chat")
        self.assertNotIn("vision_caption.rs", result.stdout)
        self.assertLess(elapsed, 10)

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

    def test_search_quotes_usable_bm25_source_before_failing_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "heartbeat.py"
            source.write_text('logger.info("managed work uses a heartbeat for automation")\n')
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\nimport time\ntime.sleep(9)\n"
                f"print(\"File: {source}, Lines: 20-20\")\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "search", "managed", "heartbeat", "invisible", "user", "automation",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=12,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            'The source shows logger.info("managed work uses a heartbeat for automation") (heartbeat.py:1).\n',
        )
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
        self.assertEqual(result.stderr, "pbi: no source locations found\n")

    def test_search_hang_fails_closed_when_candidates_lack_named_symbol(self) -> None:
        # #22: a named-symbol miss must fail closed with one exact outcome and
        # never chat; the controlled rg makes the outcome deterministic.
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
            env, trace = self.fake_environment(directory)
            env["PBI_CHAT_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print(f'File: {repo}/real.py, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            rg = directory / "rg"
            rg.write_text("#!/usr/bin/env bash\nexit 1\n")
            rg.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\ntouch \"$PBI_TEST_TRACE\"\nsleep 30\n")
            fake_chat.chmod(0o755)
            started = time.time()
            result = self.run_pbi(
                "search", f"Locate {symbol}", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.time() - started
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source location contains the queried symbol\n")
        self.assertLess(elapsed, 5, "named-symbol miss must not hang in Probe Chat")
        self.assertFalse(trace.exists(), "a named-symbol miss must skip Probe Chat")
        self.assertNotIn("pbi: probe-chat failed", result.stderr)
        self.assertNotIn("pbi: probe-chat timed out answering the question", result.stderr)

    def test_search_probe_hang_fails_closed_without_locations(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env bash\ntimeout --kill-after=1s 1s sleep 30\n")
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", "--timeout", "1", "hallucination", "caption", env=env, cwd=ROOT,
                binary=self.fake_pbi(directory, probe), timeout=20,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
        self.assertLess(elapsed, 5)

    def test_search_probe_timeout_recovers_partial_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "breaker.py"
            source.write_text("breaker_open = True\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                f"printf 'File: {source}, Lines: 1-1\\n'\n"
                "timeout --kill-after=1s 0.1s sleep 30\n"
            )
            probe.chmod(0o755)
            started = time.monotonic()
            result = self.run_pbi(
                "search", "find", "the", "breaker-open", "implementation", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=5,
            )
            elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "breaker.py:1\n")
        self.assertEqual(result.stderr, "")
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
            env, _ = self.fake_environment(directory)
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
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stderr, "pbi: no source locations found\n")
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

    def test_successful_endpoint_fallback_strips_debug_chrome(self) -> None:
        for debug in (False, True):
            with self.subTest(debug=debug), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                env, _ = self.fake_environment(directory)
                for name in ("CLIPROXY_API_KEY", "OPENAI_API_KEY", "LOCAL_ROUTER_API_KEY"):
                    env.pop(name, None)
                if debug:
                    env["DEBUG"] = "1"
                config_path = directory / ".config" / "pbi" / "config.toml"
                config_path.parent.mkdir(parents=True)
                config_path.write_text(
                    '[[endpoints]]\n'
                    'provider = "openai"\n'
                    'model = "first-model"\n'
                    'base_url = "https://first.example/v1"\n'
                    'api_key = "first-fixture-secret"\n'
                    '\n'
                    '[[endpoints]]\n'
                    'provider = "openai"\n'
                    'model = "second-model"\n'
                    'base_url = "https://second.example/v1"\n'
                    'api_key = "second-fixture-secret"\n'
                )
                fake_chat = directory / "probe-chat"
                fake_chat.write_text(
                    "#!/usr/bin/env bash\n"
                    "printf '%s\\n' '[FallbackManager] Attempting provider: first-model "
                    "(model not found) baseURL=https://first.example/v1 apiKey=first-fixture-secret'\n"
                    "printf '%s\\n' '[FallbackManager] ✅ Success with provider: second-model "
                    "baseURL=https://second.example/v1 apiKey=second-fixture-secret'\n"
                    "printf '%s\\n' 'AI SDK Warning System: To turn off warning logging, set the AI_SDK_LOG_WARNINGS global to false.'\n"
                    "printf '%s\\n' 'pong'\n"
                )
                fake_chat.chmod(0o755)
                result = self.run_pbi("--message", "CHAIN_SENTINEL reply with the single word pong", env=env)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "pong\n")
            if debug:
                self.assertIn("[FallbackManager] Attempting provider:", result.stderr)
                self.assertIn("[FallbackManager] ✅ Success with provider:", result.stderr)
                self.assertIn("[REDACTED_URL]", result.stderr)
                self.assertIn("[REDACTED]", result.stderr)
            else:
                self.assertEqual(result.stderr, "")
            self.assertNotIn("first-fixture-secret", result.stdout + result.stderr)
            self.assertNotIn("second-fixture-secret", result.stdout + result.stderr)
            self.assertNotIn("first.example", result.stdout + result.stderr)
            self.assertNotIn("second.example", result.stdout + result.stderr)


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
        self.assertIn("search_default=compact_verified_bm25_no_chat", result.stdout)
        self.assertIn("search_bm25_opt_in=--bm25_raw_no_llm_probe", result.stdout)
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


    def test_config_toml_endpoints_are_loaded_in_file_order(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                'primary_model = "ignored-by-endpoint-primary"\n'
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-primary"\n'
                'base_url = "http://primary.invalid/v1"\n'
                'api_key = "endpoint-primary-secret"\n'
                'reasoning_effort = "medium"\n'
                '\n'
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-fallback"\n'
                'base_url = "http://fallback.invalid/v1"\n'
                'api_key = "endpoint-fallback-secret"\n'
                'reasoning_effort = false\n'
            )
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("primary_model=endpoint-primary", result.stdout)
        self.assertIn("endpoint_0_model=endpoint-primary", result.stdout)
        self.assertIn("endpoint_0_base_url=http://primary.invalid/v1", result.stdout)
        self.assertIn("endpoint_1_model=endpoint-fallback", result.stdout)
        self.assertIn("endpoint_1_base_url=http://fallback.invalid/v1", result.stdout)


    def test_config_toml_endpoint_chain_forwards_distinct_identities(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            for name in ("CLIPROXY_API_KEY", "OPENAI_API_KEY", "LOCAL_ROUTER_API_KEY"):
                env.pop(name, None)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-primary"\n'
                'base_url = "http://primary.invalid/v1"\n'
                'api_key = "endpoint-primary-secret"\n'
                'reasoning_effort = "medium"\n'
                '\n'
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-fallback"\n'
                'base_url = "http://fallback.invalid/v1"\n'
                'api_key = "endpoint-fallback-secret"\n'
                'reasoning_effort = "low"\n'
            )
            result = self.run_pbi("--message", "hello", env=env)
            self.assertTrue(trace.exists(), result.stderr)
            recorded = json.loads(trace.read_text())
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertNotIn("endpoint-primary-secret", result.stdout + result.stderr)
        self.assertNotIn("endpoint-fallback-secret", result.stdout + result.stderr)
        configured = recorded["env"]
        self.assertEqual(configured["FORCE_PROVIDER"], "openai")
        self.assertEqual(configured["MODEL_NAME"], "endpoint-primary")
        self.assertEqual(configured["OPENAI_API_KEY"], "endpoint-primary-secret")
        self.assertEqual(configured["OPENAI_API_URL"], "http://primary.invalid/v1")
        self.assertEqual(
            json.loads(configured["FALLBACK_PROVIDERS"]),
            [
                {
                    "provider": "openai",
                    "apiKey": "endpoint-primary-secret",
                    "baseURL": "http://primary.invalid/v1",
                    "model": "endpoint-primary",
                    "maxRetries": 3,
                },
                {
                    "provider": "openai",
                    "apiKey": "endpoint-fallback-secret",
                    "baseURL": "http://fallback.invalid/v1",
                    "model": "endpoint-fallback",
                    "maxRetries": 0,
                },
            ],
        )


    def test_config_toml_endpoint_debug_output_redacts_every_key(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            config_path = directory / ".config" / "pbi" / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-primary"\n'
                'base_url = "http://primary.invalid/v1"\n'
                'api_key = "endpoint-primary-secret"\n'
                '\n'
                '[[endpoints]]\n'
                'provider = "openai"\n'
                'model = "endpoint-fallback"\n'
                'base_url = "http://fallback.invalid/v1"\n'
                'api_key = "endpoint-fallback-secret"\n'
            )
            result = self.run_pbi("--debug-config", env=env)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("endpoint-primary-secret", result.stdout + result.stderr)
        self.assertNotIn("endpoint-fallback-secret", result.stdout + result.stderr)
        self.assertIn("endpoint_0_api_key=[REDACTED]", result.stdout)
        self.assertIn("endpoint_1_api_key=[REDACTED]", result.stdout)


    def test_config_toml_ordinary_tables_are_ignored_without_wiping_root(self) -> None:
        cases = (
            (
                'primary_model = "spark"\n'
                '[other]\n'
                'primary_model = "shadow"\n'
            ),
            (
                'model = "spark"\n'
                '[other]\n'
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
                    self.assertIn("primary_model=spark", result.stdout)
                    self.assertNotIn("primary_model=shadow", result.stdout)
            self.assertFalse(trace.exists(), "debug config must not launch Probe Chat")


    def test_config_toml_unsafe_multiline_is_ignored_and_unknown_array_tables_are_skipped(self) -> None:
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
                    expected_model = "spark" if "[[providers]]" in config_text else PRIMARY
                    self.assertIn(f"primary_model={expected_model}", result.stdout)
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

    def test_fails_closed_with_classified_probe_chat_launch_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "delegated-sandbox"
            repo.mkdir()
            env, _ = self.fake_environment(directory)
            # Keep only the PATH fixture's helper ahead of node and core utilities.
            node_bin = os.path.dirname(shutil.which("node") or "/usr/bin/node")
            env["PATH"] = f"{directory}:{node_bin}:/usr/bin:/bin"
            helper = directory / "probe-chat"
            cases = (
                ("not-executable", "#! /usr/bin/env bash\necho should-not-run\n", 0o644, 126),
                ("interpreter-loader", "#!/definitely/missing/interpreter\n", 0o755, 127),
            )
            for category, source, mode, expected_status in cases:
                with self.subTest(category=category):
                    helper.write_text(source)
                    helper.chmod(mode)
                    result = self.run_pbi(
                        "--message",
                        "hello",
                        env=env,
                        cwd=repo,
                        binary=self.fake_pbi(directory, directory / "probe"),
                    )
                    self.assertEqual(result.returncode, expected_status, result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertIn("failed to launch", result.stderr)
                    self.assertIn(f"category={category}", result.stderr)
                    self.assertIn(f"helper={helper}", result.stderr)
                    self.assertIn("recovery=", result.stderr)
                    self.assertIn("retry once", result.stderr)
                    self.assertNotIn("CLIPROXY_API_KEY", result.stdout + result.stderr)
                    self.assertNotIn("test-key", result.stdout + result.stderr)
                    self.assertNotIn("probe-chat reported an API error", result.stderr)
                    self.assertNotIn("probe-chat failed", result.stderr)

    def test_fails_closed_with_classified_probe_chat_runtime_exit_126(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            env, _ = self.fake_environment(directory)
            helper = directory / "probe-chat"
            helper.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'runtime-output-should-not-leak'\n"
                "printf '%s\\n' 'runtime-error-should-not-leak' >&2\n"
                "exit 126\n"
            )
            helper.chmod(0o755)
            result = self.run_pbi(
                "--message",
                "hello",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, directory / "probe"),
            )
        self.assertEqual(result.returncode, 126, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn("category=runtime-exit", result.stderr)
        self.assertIn(f"helper={helper}", result.stderr)
        self.assertIn("exit=126", result.stderr)
        self.assertIn("recovery=inspect probe-chat, then retry once", result.stderr)
        self.assertNotIn("failed to launch", result.stderr)
        self.assertNotIn("runtime-output-should-not-leak", result.stdout + result.stderr)
        self.assertNotIn("runtime-error-should-not-leak", result.stdout + result.stderr)
        self.assertNotIn("test-key", result.stdout + result.stderr)


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
                self.assertNotRegex(result.stdout, r"^[^:\n]+:\d+\n?\Z")

    def make_install_source(self, directory: Path) -> tuple[Path, str]:
        checkout = directory / "source-checkout"
        checkout.mkdir()
        source = checkout / "pbi"
        shutil.copy2(PBI, source)
        source.chmod(0o755)
        subprocess.run(["git", "init", "-q"], cwd=checkout, check=True)
        git_env = os.environ.copy()
        git_env.update(
            GIT_AUTHOR_NAME="installer-test",
            GIT_AUTHOR_EMAIL="installer-test@example.invalid",
            GIT_COMMITTER_NAME="installer-test",
            GIT_COMMITTER_EMAIL="installer-test@example.invalid",
        )
        subprocess.run(["git", "add", "pbi"], cwd=checkout, check=True, env=git_env)
        subprocess.run(
            ["git", "commit", "-qm", "installer fixture"],
            cwd=checkout,
            check=True,
            env=git_env,
        )
        commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=checkout, text=True
        ).strip()
        return source, commit

    def test_installer_rejects_directory_target_without_residue(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, _ = self.make_install_source(directory)
            target = directory / "bin" / "pbi"
            target.mkdir(parents=True)
            home = directory / "home"
            result = subprocess.run(
                [str(INSTALLER), "--source", str(source), "--target", str(target), "--home", str(home)],
                text=True, capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(target.is_dir())
            # Residue must be sought beside the obstruction (owning parents),
            # never inside the obstructing directory itself.
            self.assertFalse(list(target.parent.glob(".pbi.*")))
            self.assertFalse(list((home / ".local" / "bin").glob(".pbi.*")))
            self.assertFalse(target.with_name("pbi.provenance").exists())
            self.assertFalse((home / ".local" / "bin" / "pbi").exists())

    def test_installer_rolls_back_late_publication_failures(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, _ = self.make_install_source(directory)
            target = directory / "bin" / "pbi"
            home = directory / "home"
            command = [str(INSTALLER), "--source", str(source), "--target", str(target), "--home", str(home)]
            first = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            provenance = target.with_name("pbi.provenance")
            link = home / ".local" / "bin" / "pbi"
            old_target = target.read_bytes()
            old_provenance = provenance.read_bytes()
            old_link = os.readlink(link)
            source.write_bytes(old_target + b"# v2\n")
            source.chmod(0o755)

            with self.subTest(obstruction="compatibility directory"):
                link.unlink()
                link.mkdir()
                failed = subprocess.run(command, text=True, capture_output=True)
                self.assertNotEqual(failed.returncode, 0)
                self.assertEqual(target.read_bytes(), old_target)
                self.assertEqual(provenance.read_bytes(), old_provenance)
                self.assertTrue(link.is_dir())
                self.assertFalse(list(target.parent.glob(".pbi.*")))
                self.assertFalse(list(link.parent.glob(".pbi.*")))
                link.rmdir()
                link.symlink_to(old_link)

            with self.subTest(obstruction="provenance publish"):
                fake_bin = directory / "fake-bin"
                fake_bin.mkdir()
                fake_mv = fake_bin / "mv"
                fake_mv.write_text(
                    "#!/bin/sh\n"
                    f"case \"$*\" in *'.pbi.provenance.tmp.'*' {provenance}') echo obstructed >&2; exit 1;; esac\n"
                    "exec /usr/bin/mv \"$@\"\n"
                )
                fake_mv.chmod(0o755)
                env = os.environ | {"PATH": f"{fake_bin}:{os.environ['PATH']}"}
                failed = subprocess.run(command, text=True, capture_output=True, env=env)
                self.assertNotEqual(failed.returncode, 0)
                self.assertEqual(target.read_bytes(), old_target)
                self.assertEqual(provenance.read_bytes(), old_provenance)
                self.assertEqual(os.readlink(link), old_link)
                self.assertFalse(list(target.parent.glob(".pbi.*")))
                self.assertFalse(list(link.parent.glob(".pbi.*")))

    def test_installer_copy_failure_before_transaction_preserves_prior_identities(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, _ = self.make_install_source(directory)
            target = directory / "bin" / "pbi"
            provenance = target.with_name("pbi.provenance")
            home = directory / "home"
            link = home / ".local" / "bin" / "pbi"
            command = [str(INSTALLER), "--source", str(source), "--target", str(target), "--home", str(home)]
            installed = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(installed.returncode, 0, installed.stderr)
            prior = {
                "target": (target.read_bytes(), (target.stat().st_dev, target.stat().st_ino)),
                "provenance": (provenance.read_bytes(), (provenance.stat().st_dev, provenance.stat().st_ino)),
                "link": (os.readlink(link), (link.lstat().st_dev, link.lstat().st_ino)),
            }
            fake_bin = directory / "copy-failure-bin"
            fake_bin.mkdir()
            failing_install = fake_bin / "install"
            failing_install.write_text(
                "#!/bin/sh\n"
                "for destination in \"$@\"; do :; done\n"
                "printf partial >\"$destination\"\n"
                "exit 1\n"
            )
            failing_install.chmod(0o755)
            failed = subprocess.run(
                command,
                text=True,
                capture_output=True,
                env=os.environ | {"PATH": f"{fake_bin}:{os.environ['PATH']}"},
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(target.read_bytes(), prior["target"][0])
            self.assertEqual(provenance.read_bytes(), prior["provenance"][0])
            self.assertEqual(os.readlink(link), prior["link"][0])
            self.assertEqual((target.stat().st_dev, target.stat().st_ino), prior["target"][1])
            self.assertEqual((provenance.stat().st_dev, provenance.stat().st_ino), prior["provenance"][1])
            self.assertEqual((link.lstat().st_dev, link.lstat().st_ino), prior["link"][1])
            self.assertFalse(list(target.parent.glob(".pbi.*")))
            self.assertFalse(list(link.parent.glob(".pbi.*")))

    def test_installer_rejects_target_link_alias_before_mutation(self) -> None:
        # #117: a target that aliases the compatibility path (lexically equal or
        # via a symlinked parent) must be rejected before any mutation with an
        # exact diagnostic and zero residue; it must never publish a
        # self-referential link over the stable executable.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, _ = self.make_install_source(directory)
            home = directory / "home"
            lexical_target = home / ".local" / "bin" / "pbi"
            same_dir = directory / "same"
            alias_home = directory / "alias-home"
            (alias_home / ".local").mkdir(parents=True)
            (same_dir).mkdir()
            (alias_home / ".local" / "bin").symlink_to(same_dir)
            symlink_alias_target = same_dir / "pbi"
            command = [str(INSTALLER), "--source", str(source), "--target", "TARGET", "--home", "HOME"]
            for label, target in (
                ("lexical equality", lexical_target),
                ("symlinked-parent alias", symlink_alias_target),
            ):
                with self.subTest(alias=label):
                    result = subprocess.run(
                        [c.replace("TARGET", str(target)).replace("HOME", str(home)) for c in command]
                        if label == "lexical equality" else
                        [c.replace("TARGET", str(target)).replace("HOME", str(alias_home)) for c in command],
                        text=True, capture_output=True,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(
                        result.stderr,
                        f"install.sh: target and compatibility path are the same file: {target}\n",
                    )
                    self.assertFalse(os.path.lexists(target), "aliased target must stay absent")
                    self.assertFalse(os.path.lexists(target.with_name("pbi.provenance")))
                    self.assertFalse(list(target.parent.glob(".pbi.*")))
        self.assertFalse(list((home / ".local" / "bin").glob(".pbi.*")))

    def test_installer_upgrade_rename_never_hides_the_target(self) -> None:
        # #117: an upgrading install must preserve the live target bytes
        # (same-directory backup, no rename-away) and atomically rename the
        # staged executable over the leaf; an observer at every public rename
        # must always see a regular executable that is wholly old or new.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, _ = self.make_install_source(directory)
            target = directory / "bin" / "pbi"
            home = directory / "home"
            command = [str(INSTALLER), "--source", str(source), "--target", str(target), "--home", str(home)]
            first = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            old_sha = hashlib.sha256(target.read_bytes()).hexdigest()
            source.write_bytes(source.read_bytes() + b"# v2\n")
            source.chmod(0o755)
            fake_bin = directory / "observer-bin"
            fake_bin.mkdir()
            log = directory / "mv-observations.log"
            fake_mv = fake_bin / "mv"
            fake_mv.write_text(
                "#!/usr/bin/env python3\n"
                "import hashlib, os, pathlib, subprocess, sys\n"
                "target = os.environ['PBI_OBSERVER_TARGET']\n"
                "log = os.environ['PBI_OBSERVER_LOG']\n"
                "def describe(path):\n"
                "    try:\n"
                "        os.lstat(path)\n"
                "    except FileNotFoundError:\n"
                "        return 'absent'\n"
                "    if os.path.islink(path):\n"
                "        return 'symlink:' + os.readlink(path)\n"
                "    if not os.path.isfile(path):\n"
                "        return 'other'\n"
                "    return 'regular:' + hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()\n"
                "destination = sys.argv[-1]\n"
                "if os.path.abspath(destination) == os.path.abspath(target):\n"
                "    with open(log, 'a') as f:\n"
                "        f.write('pre ' + describe(destination) + '\\n')\n"
                "result = subprocess.run(['/usr/bin/mv', *sys.argv[1:]])\n"
                "if os.path.abspath(destination) == os.path.abspath(target):\n"
                "    with open(log, 'a') as f:\n"
                "        f.write('post ' + describe(destination) + '\\n')\n"
                "raise SystemExit(result.returncode)\n"
            )
            fake_mv.chmod(0o755)
            env = os.environ | {
                "PATH": f"{fake_bin}:{os.environ['PATH']}",
                "PBI_OBSERVER_TARGET": str(target),
                "PBI_OBSERVER_LOG": str(log),
            }
            upgraded = subprocess.run(command, text=True, capture_output=True, env=env)
            self.assertEqual(upgraded.returncode, 0, upgraded.stderr)
            new_sha = hashlib.sha256(target.read_bytes()).hexdigest()
            observations = log.read_text().splitlines()
            self.assertTrue(observations, "the public target rename must be observed")
            for line in observations:
                kind, state = line.split(" ", 1)
                self.assertIn(kind, ("pre", "post"))
                self.assertTrue(
                    state == f"regular:{old_sha}" or state == f"regular:{new_sha}",
                    f"public target must always be wholly old or new bytes, got: {state!r}",
                )
            self.assertEqual(new_sha, hashlib.sha256(source.read_bytes()).hexdigest())

    def test_installer_signals_restore_exact_prior_or_absent_state(self) -> None:
        seams = (
            ("target backup", "target_backup", True),
            ("target publish", "target_publish", False),
            ("provenance backup", "provenance_backup", True),
            ("provenance publish", "provenance_publish", False),
            ("link backup", "link_backup", True),
            ("link publish", "link_publish", False),
        )
        for initially_existing in (False, True):
            for seam, signal_kind, existing_only in seams:
                if existing_only and not initially_existing:
                    continue
                with self.subTest(initially_existing=initially_existing, seam=seam), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    source, _ = self.make_install_source(directory)
                    target = directory / "bin" / "pbi"
                    provenance = target.with_name("pbi.provenance")
                    home = directory / "home"
                    link = home / ".local" / "bin" / "pbi"
                    command = [str(INSTALLER), "--source", str(source), "--target", str(target), "--home", str(home)]
                    prior: dict[str, tuple[bytes | str, tuple[int, int]]] = {}
                    if initially_existing:
                        installed = subprocess.run(command, text=True, capture_output=True)
                        self.assertEqual(installed.returncode, 0, installed.stderr)
                        prior = {
                            "target": (target.read_bytes(), (target.stat().st_dev, target.stat().st_ino)),
                            "provenance": (provenance.read_bytes(), (provenance.stat().st_dev, provenance.stat().st_ino)),
                            "link": (os.readlink(link), (link.lstat().st_dev, link.lstat().st_ino)),
                        }
                        source.write_bytes(source.read_bytes() + b"# signal-upgrade\n")
                        source.chmod(0o755)

                    fake_bin = directory / "signal-bin"
                    fake_bin.mkdir()
                    hit = directory / "signal-hit"
                    fake_mv = fake_bin / "mv"
                    fake_mv.write_text(
                        "#!/usr/bin/env python3\n"
                        "import os, signal, subprocess, sys, time\n"
                        "result = subprocess.run(['/usr/bin/mv', *sys.argv[1:]])\n"
                        "source, destination = sys.argv[-2:]\n"
                        "public = {'target': os.environ['PBI_SIGNAL_TARGET'], 'provenance': os.environ['PBI_SIGNAL_PROVENANCE'], 'link': os.environ['PBI_SIGNAL_LINK']}\n"
                        "name, operation = os.environ['PBI_SIGNAL_KIND'].split('_')\n"
                        "matches = (source == public[name] and destination != public[name]) if operation == 'backup' else (source != public[name] and destination == public[name])\n"
                        "if result.returncode == 0 and matches and not os.path.exists(os.environ['PBI_SIGNAL_HIT']):\n"
                        "    open(os.environ['PBI_SIGNAL_HIT'], 'w').write(source + '\\n' + destination + '\\n')\n"
                        "    os.kill(os.getppid(), signal.SIGTERM)\n"
                        "    time.sleep(.05)\n"
                        "raise SystemExit(result.returncode)\n"
                    )
                    fake_mv.chmod(0o755)
                    # The target's prior bytes are now preserved by a hard
                    # link, not a rename; inject the same TERM at that seam.
                    fake_ln = fake_bin / "ln"
                    fake_ln.write_text(
                        "#!/usr/bin/env python3\n"
                        "import os, signal, subprocess, sys, time\n"
                        "result = subprocess.run(['/usr/bin/ln', *sys.argv[1:]])\n"
                        "name, operation = os.environ['PBI_SIGNAL_KIND'].split('_')\n"
                        "if operation == 'backup' and name == 'target':\n"
                        "    public = os.environ['PBI_SIGNAL_TARGET']\n"
                        "    source, destination = sys.argv[-2:]\n"
                        "    matches = (source == public and destination != public)\n"
                        "    if result.returncode == 0 and matches and not os.path.exists(os.environ['PBI_SIGNAL_HIT']):\n"
                        "        open(os.environ['PBI_SIGNAL_HIT'], 'w').write(source + '\\n' + destination + '\\n')\n"
                        "        os.kill(os.getppid(), signal.SIGTERM)\n"
                        "        time.sleep(.05)\n"
                        "raise SystemExit(result.returncode)\n"
                    )
                    fake_ln.chmod(0o755)
                    env = os.environ | {
                        "PATH": f"{fake_bin}:{os.environ['PATH']}",
                        "PBI_SIGNAL_KIND": signal_kind,
                        "PBI_SIGNAL_TARGET": str(target),
                        "PBI_SIGNAL_PROVENANCE": str(provenance),
                        "PBI_SIGNAL_LINK": str(link),
                        "PBI_SIGNAL_HIT": str(hit),
                    }
                    interrupted = subprocess.run(command, text=True, capture_output=True, env=env)
                    self.assertTrue(hit.exists(), "the requested post-rename signal seam must be reached")
                    self.assertNotEqual(interrupted.returncode, 0, "a signal must never report installer success")
                    if initially_existing:
                        self.assertEqual(target.read_bytes(), prior["target"][0])
                        self.assertEqual(provenance.read_bytes(), prior["provenance"][0])
                        self.assertEqual(os.readlink(link), prior["link"][0])
                        self.assertEqual((target.stat().st_dev, target.stat().st_ino), prior["target"][1])
                        self.assertEqual((provenance.stat().st_dev, provenance.stat().st_ino), prior["provenance"][1])
                        self.assertEqual((link.lstat().st_dev, link.lstat().st_ino), prior["link"][1])
                    else:
                        self.assertFalse(os.path.lexists(target))
                        self.assertFalse(os.path.lexists(provenance))
                        self.assertFalse(os.path.lexists(link))
                    self.assertFalse(list(target.parent.glob(".pbi.*")))
                    self.assertFalse(list(link.parent.glob(".pbi.*")))

    def test_installer_is_durable_and_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source, commit = self.make_install_source(directory)
            target = directory / "usr" / "local" / "bin" / "pbi"
            home = directory / "home"
            command = [
                str(INSTALLER),
                "--source",
                str(source),
                "--target",
                str(target),
                "--home",
                str(home),
            ]
            env = os.environ.copy()
            env["CLIPROXY_API_KEY"] = "installer-secret-must-not-leak"
            first = subprocess.run(command, text=True, capture_output=True, env=env)
            self.assertEqual(first.returncode, 0, first.stderr)
            source_bytes = source.read_bytes()
            expected_sha = hashlib.sha256(source_bytes).hexdigest()
            provenance = target.with_name("pbi.provenance")
            self.assertTrue(target.is_file())
            self.assertFalse(target.is_symlink())
            self.assertTrue(os.access(target, os.X_OK))
            self.assertEqual(target.read_bytes(), source_bytes)
            self.assertEqual(hashlib.sha256(target.read_bytes()).hexdigest(), expected_sha)
            self.assertTrue(provenance.is_file())
            provenance_text = provenance.read_text()
            self.assertEqual(
                provenance_text,
                f"source_commit={commit}\nsha256={expected_sha}\ntarget={target}\n",
            )
            link = home / ".local" / "bin" / "pbi"
            self.assertTrue(link.is_symlink())
            self.assertEqual(link.resolve(), target.resolve())

            second = subprocess.run(command, text=True, capture_output=True, env=env)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual(target.read_bytes(), source_bytes)
            self.assertEqual(link.resolve(), target.resolve())

            previous_target = target.read_bytes()
            failing_bin = directory / "failing-bin"
            failing_bin.mkdir()
            failing_install = failing_bin / "install"
            failing_install.write_text(
                "#!/bin/sh\n"
                "for destination in \"$@\"; do :; done\n"
                "printf partial >\"$destination\"\n"
                "printf '%s\\n' 'copy failed' >&2\n"
                "exit 1\n"
            )
            failing_install.chmod(0o755)
            failure_env = env | {"PATH": f"{failing_bin}:{env['PATH']}"}
            failed = subprocess.run(command, text=True, capture_output=True, env=failure_env)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(target.read_bytes(), previous_target)
            self.assertEqual(link.resolve(), target.resolve())
            self.assertFalse(list(target.parent.glob(".pbi.*tmp.*")))
            self.assertIn("copy failed", failed.stderr)

            shutil.rmtree(source.parent)
            version = subprocess.run([str(target), "--version"], text=True, capture_output=True)
            self.assertEqual(version.returncode, 0, version.stderr)
            self.assertIn("pbi", version.stdout)

    def test_multi_target_where_list_rejects_singleton_location(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "workflow.rs"
            source.write_text(
                "fn alpha_scoped_concurrency_marker_consumption_exclusive_route_conflict_checkpoint_join_state() {}\n"
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'workflow.rs:1'\\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "Where are alpha-scoped concurrency, marker consumption, exclusive route conflict, and checkpoint join state implemented?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0, (result.stdout, result.stderr))
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr)

    def test_multi_target_where_pair_rejects_singleton_location(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "retry.rs"
            source.write_text(
                "fn failure_matrix_metadata_selector_semantics_retry_budget_invalid_output_publication() {}\n"
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'retry.rs:1'\\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where are failure matrix metadata and selector semantics implemented for retry budget invalid-output publication?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0, (result.stdout, result.stderr))
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr)

    def test_explicit_symbol_relationship_query_rejects_singleton_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "source.rs"
            source.write_text("const STATE_001: &str = \"unrelated state key\";\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text("#!/usr/bin/env python3\nprint('missing.rs:1')\n")
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'source.rs:1'\\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "Where does the NodeOutput API symbol get called, and which tests cover its callers?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0, (result.stdout, result.stderr))
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr)

    def test_production_ownership_and_integration_tests_query_rejects_singleton_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "production_profile.rs"
            source.write_text("fn production_path_owns_retry_budget_and_publication_in_integration_tests() {}\n")
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text("#!/usr/bin/env bash\nprintf '%s\\n' 'production_profile.rs:1'\n")
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "What production path owns retry budget and publication in integration tests?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0, (result.stdout, result.stderr))
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr)

    def test_explicit_symbol_relationship_query_rejects_unrelated_citation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "source.rs"
            source.write_text(
                "//! API symbol caller tests module documentation.\n"
            )
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            result = self.run_pbi(
                "Where does the NodeOutput API symbol get called, and which tests cover its callers?",
                env=env,
                cwd=repo,
                binary=self.fake_pbi(directory, probe),
                timeout=8,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr)

    def test_planner_timeout_recovers_existing_bm25_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            source = repo / "audit.py"
            source.write_text("def audit_results():\n    return 'append audit'\n")
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {source}, Lines: 1-1')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys, time\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'a') as trace:\n"
                "    trace.write('planner-timeout\\n')\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "if message.startswith('Convert the code question'):\n"
                "    time.sleep(30)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where", "is", "append", "audit", env=env, cwd=repo,
                binary=self.fake_pbi(directory, probe), timeout=8,
            )
            self.assertTrue(trace.exists(), "planner timeout must be exercised")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "audit.py:1\n")
        self.assertEqual(result.stderr, "")

if __name__ == "__main__":
    unittest.main(verbosity=2)
