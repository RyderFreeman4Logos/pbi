"""Probe Chat error envelopes are untrusted, not source-search misses."""

import json
import tempfile
from pathlib import Path

import pytest

import test_pbi


@pytest.mark.parametrize("exit_code", [0, 1])
@pytest.mark.parametrize(
    "payload, expected",
    [
        ({"status": "error"}, "class=unknown stage=answer status=error"),
        ({"error": {"code": "invalid_request", "statusCode": 400}},
         "class=provider stage=answer status=invalid_request http_status=400"),
        ({"error": {"code": "ECONNREFUSED"}},
         "class=transport stage=answer status=ECONNREFUSED"),
        ({"error": {"name": "AI_TypeValidationError"}},
         "class=protocol stage=answer status=AI_TypeValidationError"),
        ({"error": {"code": "sk-secret", "statusCode": "Bearer-secret",
                    "message": "HTTP 401 api_key=sk-secret", "request_id": "sk-secret"},
          "status": "error", "id": "session-secret"},
         "class=unknown stage=answer status=error"),
        ({"error": "HTTP 503 Authorization: Bearer sk-secret"},
         "class=unknown stage=answer status=error"),
        ({"error": {"status": 429, "requestId": "req_real"}},
         "class=provider stage=answer status=error http_status=429 request=req_real"),
    ],
)
def test_typed_probe_error(payload: dict, expected: str, exit_code: int) -> None:
    harness = test_pbi.PbiTest()
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        env, _ = harness.fake_environment(directory)
        # Emulate the real child boundary, including stderr-only failures and
        # a successful process that returned an error envelope as its answer.
        (directory / "probe-chat").write_text(
            "#!/usr/bin/env python3\nimport sys\n"
            f"print({json.dumps(payload)!r}, file=sys.{'stderr' if exit_code else 'stdout'})\n"
            f"raise SystemExit({exit_code})\n"
        )
        result = harness.run_pbi("--message", "hello", env=env)
    assert result.returncode == 1
    assert result.stdout == ""
    assert result.stderr == f"pbi: probe-chat reported an API error ({expected})\n"
    assert "no source" not in result.stderr


def test_error_diagnostic_does_not_attribute_unrelated_success_metadata() -> None:
    harness = test_pbi.PbiTest()
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        env, _ = harness.fake_environment(directory)
        (directory / "probe-chat").write_text(
            "#!/usr/bin/env python3\nimport sys\n"
            "print('{\"status\":\"completed\",\"http_status\":503,\"request_id\":\"req_unrelated\"}')\n"
            "print('model not found: Bearer sk-secret', file=sys.stderr)\n"
            "raise SystemExit(1)\n"
        )
        result = harness.run_pbi("--message", "hello", env=env)
    assert result.returncode == 1
    assert result.stdout == ""
    assert result.stderr == (
        "pbi: probe-chat reported an API error "
        "(class=unknown stage=answer status=error)\n"
    )


@pytest.mark.parametrize("has_hit", [False, True])
def test_bm25_controls_do_not_become_provider_errors(has_hit: bool) -> None:
    harness = test_pbi.PbiTest()
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        env, trace = harness.fake_environment(directory)
        repo = directory / "repo"
        repo.mkdir()
        if has_hit:
            (repo / "real.py").write_text("def cleanup_receipt():\n    return True\n")
            (directory / "probe").write_text(
                "#!/usr/bin/env python3\n"
                f"print('File: {repo}/real.py, Lines: 1-2')\n"
            )
        # If source-only recovery calls chat, its fixture would fail.
        result = harness.run_pbi("search", "cleanup_receipt", env=env, cwd=repo)
        assert not trace.exists()
    if has_hit:
        assert result.returncode == 0
        assert result.stdout == "real.py:1\n"
        assert result.stderr == ""
    else:
        assert result.returncode == 1
        assert result.stdout == ""
        assert "no source" in result.stderr
        assert "API error" not in result.stderr
