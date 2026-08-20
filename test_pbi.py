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

    def test_term_resistant_initial_planner_times_out_to_direct_bm25(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os, time\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
                "    f.write('invoked')\n"
                "time.sleep(30)\n"
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
            try:
                result = self.run_pbi(
                    "where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe), timeout=4
                )
            except subprocess.TimeoutExpired as error:
                self.fail(f"initial planner timeout invoked the probe: {error}")
            elapsed = time.monotonic() - started
            planner = json.loads(trace.read_text())
            self.assertFalse((directory / "probe-trace.json").exists())
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(elapsed, 3)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "pbi: planner timed out before producing a source answer\n")
        self.assertNotIn("LICENSE:1", result.stdout + result.stderr)
        self.assertNotIn("Repository files:", planner["argv"][5])

    def test_term_resistant_refinement_planner_times_out_to_direct_bm25(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "print(f'File: {os.getcwd()}/LICENSE, Lines: 1-10')\n"
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

    def test_default_query_fails_closed_when_answer_has_no_usable_text(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env bash\n"
                f"printf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
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

    def test_question_stamp_per_line_narrative_answer_still_succeeds(self) -> None:
        # #12: a real compact answer (narrative + citation) still prints, even
        # when it includes a `path:1`-style citation line.
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
                "elif message.startswith('Identify missing evidence'):\n"
                "    print('NONE')\n"
                "else:\n"
                "    print('The entrypoint is pbi:9.')\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint",
                env=env,
                cwd=ROOT,
                binary=self.fake_pbi(directory, directory / "probe"),
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "The entrypoint is pbi:9.\n")

    def test_no_colon_warning_only_stdout_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            probe = directory / "probe"
            probe.write_text(f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-10'\n")
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
                f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-10'\n"
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
        self.assertEqual(configured["PROBE_BINARY_PATH"], PROBE_SHIM)
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
        self.assertNotEqual(wrong.returncode, 0)
        self.assertEqual(wrong.stdout, "")
        self.assertIn("contains the queried symbol", wrong.stderr)
        self.assertEqual(right.returncode, 0, right.stderr)
        self.assertEqual(right.stdout, "real.py:5\n")
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

    def test_search_hang_fails_closed_when_candidates_lack_named_symbol(self) -> None:
        # #22: a timed-out search must not turn unrelated BM25 candidates into success.
        symbol = "ingest_receipt_accepts_cleanup_ids_from_legacy_wire_shape"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repo = directory / "repo"
            repo.mkdir()
            (repo / "real.py").write_text("def unrelated():\n    return True\n")
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
        self.assertEqual(result.returncode, 23, result.stderr)
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
        self.assertEqual(result.returncode, 23, result.stderr)
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
