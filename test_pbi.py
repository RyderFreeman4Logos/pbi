#!/usr/bin/env python3
"""Focused hermetic checks for the pbi Probe Chat wrapper."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(binary), *args],
            text=True,
            capture_output=True,
            check=False,
            env=env,
            cwd=cwd,
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
        self.assertIn("--prompt", draft["argv"])
        answer_prompt = draft["argv"][draft["argv"].index("--prompt") + 1]
        self.assertIn("3 to 8 single-line Markdown bullets", answer_prompt)
        self.assertIn("The first character of the answer must be -", answer_prompt)
        self.assertIn("No title, preamble, conclusion, or repeated reference list", answer_prompt)
        self.assertIn("final - Uncertain: bullet", answer_prompt)
        self.assertIn("Review and compress the draft answer", reviewer["argv"][5])
        self.assertIn("Draft answer:\nThe entrypoint is pbi:1.", reviewer["argv"][5])
        self.assertIn("Evidence:\nFile:", reviewer["argv"][5])
        review_prompt = reviewer["argv"][reviewer["argv"].index("--prompt") + 1]
        self.assertIn("never more than 8 lines total", review_prompt)
        self.assertIn("Discard tangential examples", review_prompt)
        self.assertEqual(reviewer["argv"][-1], "--json")
        self.assertIn("Audit every source citation", citation_auditor["argv"][5])
        self.assertIn("Answer to audit:\nThe entrypoint is pbi:1.", citation_auditor["argv"][5])
        self.assertIn("Source evidence:\nFile:", citation_auditor["argv"][5])
        self.assertEqual(citation_auditor["argv"][-1], "--json")

    def test_query_planning_omits_inventory_and_times_out_to_direct_bm25(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, trace = self.fake_environment(directory)
            env["PBI_PLANNER_TIMEOUT_SECONDS"] = "1"
            probe = directory / "probe"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['PBI_TEST_PROBE_TRACE'], 'w') as f:\n"
                "    json.dump(sys.argv[1:], f)\n"
                "print(f'File: {os.getcwd()}/pbi, Lines: 1-10')\n"
            )
            probe.chmod(0o755)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys, time\n"
                "message = sys.argv[sys.argv.index('--message') + 1]\n"
                "with open(os.environ['PBI_TEST_TRACE'], 'w') as f:\n"
                "    json.dump({'argv': sys.argv[1:]}, f)\n"
                "if message.startswith('Convert the code question'):\n"
                "    time.sleep(2)\n"
                "    print('delayed query')\n"
                "else:\n"
                "    raise SystemExit(2)\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi(
                "where is the entrypoint", env=env, cwd=ROOT, binary=self.fake_pbi(directory, probe)
            )
            planner = json.loads(trace.read_text())
            probe_argv = json.loads((directory / "probe-trace.json").read_text())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "pbi:1\n")
        self.assertNotIn("Repository files:", planner["argv"][5])
        self.assertEqual(probe_argv[-1], "where is the entrypoint")

    def test_query_planning_reports_system_message_rejection(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            fake_chat = directory / "probe-chat"
            fake_chat.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' 'AI SDK Warning: System messages are risky.'\n"
            )
            fake_chat.chmod(0o755)
            result = self.run_pbi("where is the entrypoint", env=env, binary=self.fake_pbi(directory, directory / "probe"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("system-message field", result.stderr)
        self.assertIn("OpenAI-compatible backend", result.stderr)

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
        self.assertEqual(chat_recorded["argv"][8], "--prompt")
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
                "--prompt",
                "Select locations only from the supplied candidates. Do not call tools.",
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

    def test_search_falls_back_to_absolute_retrieved_location_from_symlinked_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            env, _ = self.fake_environment(directory)
            cwd = directory / "repo"
            cwd.symlink_to(ROOT, target_is_directory=True)
            env["PWD"] = str(cwd)
            probe = directory / "probe"
            probe.write_text(
                f"#!/usr/bin/env bash\nprintf '%s\\n' 'File: {PBI}, Lines: 1-292'\n"
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
        self.assertEqual(result.stdout, "pbi:1\n")
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
        self.assertEqual(json.loads(result.stdout)["error"]["code"], "invalid_request")

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
