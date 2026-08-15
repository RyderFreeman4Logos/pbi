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
        self, *args: str, env: dict[str, str] | None = None, cwd: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(PBI), *args],
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
        env["PBI_TEST_PROBE"] = str(fake_probe)
        env["HOME"] = str(directory)
        env["CLIPROXY_API_KEY"] = "test-key"
        env["MAX_RETRIES"] = "1"
        return env, trace

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
        self.assertEqual(no_args_result.returncode, 2)
        self.assertIn("--message", no_args_result.stderr)

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

    def test_search_delegates_to_probe_without_an_api_key(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            env = os.environ.copy()
            env["HOME"] = temporary
            env.pop("CLIPROXY_API_KEY", None)
            env.pop("OPENAI_API_KEY", None)
            result = self.run_pbi(
                "search", "PBI_VERSION", "--format", "plain", "--max-results", "1", env=env, cwd=ROOT
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("File:", result.stdout)
        self.assertIn("/pbi/pbi", result.stdout)

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
        self.assertIn("api_key=[REDACTED]", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
