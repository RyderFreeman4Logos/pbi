"""Approved Qwen routes prefer the exact local MP transport."""
import json

import pytest

import test_pbi

BASE = "http://gb10:18009/v1"
LOCAL_MP = "http://localhost:18317/v1"
MODELS = tuple(f"abliterated-qwen-latest-27b-{level}" for level in ("none", "low", "medium"))


def endpoint(model, base=BASE, provider="openai"):
    return (f'[[endpoints]]\nprovider = "{provider}"\nmodel = "{model}"\n'
            f'base_url = "{base}"\napi_key = "fixture-secret"\n')


def test_message_cannot_override_admitted_model(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    env.update(LOCAL_MODEL=MODELS[0], LOCAL_ROUTER_BASEURL=BASE, FALLBACK_MODEL=MODELS[0])
    result = harness.run_pbi("--message", "hello", "--model-name", "spark",
                             "--force-provider=anthropic", env=env)
    assert result.returncode == 23, result.stderr
    argv = json.loads(trace.read_text())["argv"]
    assert "spark" not in argv and "--force-provider=anthropic" not in argv


def test_hostile_endpoint_chain_never_reaches_chat(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{MODELS[0]}"\n' + "".join(
        endpoint(model, "http://127.0.0.1:8317/v1")
        for model in ("spark", MODELS[0], "gpt-6-luna", "opencode/deepseek-v4-flash-skip")))
    env.update(PBI_CONFIG_FILE=str(config), LOCAL_ROUTER_BASEURL=BASE)
    result = harness.run_pbi("--message", "hello", env=env)
    assert result.returncode == 23, result.stderr
    route = json.loads(trace.read_text())["env"]
    assert route["MODEL_NAME"] == MODELS[0]
    assert route["OPENAI_API_URL"] == BASE
    slots = json.loads(route["FALLBACK_PROVIDERS"])
    assert slots and all(slot["model"] in MODELS and slot["baseURL"] == BASE
                         and slot["provider"] == "openai" for slot in slots)
    debug = harness.run_pbi("--debug-config", env=env)
    assert debug.returncode == 0, debug.stderr
    assert f'primary_model={route["MODEL_NAME"]}\n' in debug.stdout
    assert f'endpoint_count={len(slots)}\n' in debug.stdout
    for index, slot in enumerate(slots):
        assert f'endpoint_{index}_model={slot["model"]}\n' in debug.stdout
        assert f'endpoint_{index}_base_url={slot["baseURL"]}\n' in debug.stdout
    assert "fixture-secret" not in debug.stdout + debug.stderr
    assert "[REDACTED]" in debug.stdout
    assert all(model not in debug.stdout for model in ("spark", "gpt-6", "deepseek", "8317"))


def test_wrong_root_primary_exits_before_chat(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text('primary_model = "spark"\n' + endpoint(MODELS[0]))
    env.update(PBI_CONFIG_FILE=str(config))
    for args in (("--message", "hello"), ("--debug-config",)):
        result = harness.run_pbi(*args, env=env)
        assert result.returncode == 78
        assert "phase=routing category=unapproved-local-route" in result.stderr
        assert result.stdout == ""
        assert "fixture-secret" not in result.stderr
        assert not trace.exists()


def test_issue_config_debug_admits_only_approved_none(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{MODELS[0]}"\n' + "".join(
        endpoint(model, "http://127.0.0.1:8317/v1")
        for model in ("spark", MODELS[0], "gpt-6-luna", "opencode/deepseek-v4-flash-skip")))
    env.update(PBI_CONFIG_FILE=str(config), LOCAL_ROUTER_BASEURL=BASE)
    debug = harness.run_pbi("--debug-config", env=env)
    assert debug.returncode == 0, debug.stderr
    assert f"primary_model={MODELS[0]}\n" in debug.stdout
    assert f"base_url={BASE}\n" in debug.stdout
    assert "endpoint_count=1\n" in debug.stdout
    assert f"endpoint_0_model={MODELS[0]}\n" in debug.stdout
    assert f"endpoint_0_base_url={BASE}\n" in debug.stdout
    assert "endpoint_1_" not in debug.stdout
    assert "fixture-secret" not in debug.stdout + debug.stderr
    assert "[REDACTED]" in debug.stdout
    assert all(token not in debug.stdout for token in ("spark", "gpt-6", "deepseek", "8317"))
    assert not trace.exists()


@pytest.mark.parametrize("model", MODELS)
def test_exact_local_catalog_models_can_be_selected(tmp_path, model):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{model}"\n' + "".join(endpoint(m) for m in reversed(MODELS)))
    env["PBI_CONFIG_FILE"] = str(config)
    result = harness.run_pbi("--message", "hello", env=env)
    assert result.returncode == 23, result.stderr
    route = json.loads(trace.read_text())["env"]
    assert route["MODEL_NAME"] == model
    assert route["OPENAI_API_URL"] == BASE
    assert all(slot["model"] in MODELS and slot["baseURL"] == BASE
               for slot in json.loads(route["FALLBACK_PROVIDERS"]))


@pytest.mark.parametrize("model", MODELS)
def test_approved_models_accept_exact_local_mp_transport(tmp_path, model):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{model}"\n' + "".join(
        endpoint(approved, LOCAL_MP) for approved in reversed(MODELS)))
    env["PBI_CONFIG_FILE"] = str(config)

    result = harness.run_pbi("--message", "hello", env=env)

    assert result.returncode == 23, result.stderr
    route = json.loads(trace.read_text())["env"]
    assert route["MODEL_NAME"] == model
    assert route["OPENAI_API_URL"] == LOCAL_MP
    slots = json.loads(route["FALLBACK_PROVIDERS"])
    assert [slot["model"] for slot in slots] == [model] + [m for m in reversed(MODELS) if m != model]
    assert all(slot["provider"] == "openai" and slot["baseURL"] == LOCAL_MP for slot in slots)


@pytest.mark.parametrize("model", MODELS)
def test_explicit_unapproved_configured_base_is_not_overridden(tmp_path, model):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{model}"\n' + "".join(
        endpoint(approved, "http://configured-route.invalid/v1")
        for approved in reversed(MODELS)))
    env["PBI_CONFIG_FILE"] = str(config)

    result = harness.run_pbi("--message", "hello", env=env)

    assert result.returncode == 78
    assert "phase=routing category=unapproved-local-route" in result.stderr
    assert result.stdout == ""
    assert "fixture-secret" not in result.stdout + result.stderr
    assert not trace.exists()


@pytest.mark.parametrize("model", MODELS)
def test_missing_configured_base_defaults_to_local_mp(tmp_path, model):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{model}"\n' + "".join(
        endpoint(approved, "") for approved in reversed(MODELS)))
    env["PBI_CONFIG_FILE"] = str(config)

    result = harness.run_pbi("--message", "hello", env=env)

    assert result.returncode == 23, result.stderr
    route = json.loads(trace.read_text())["env"]
    assert route["MODEL_NAME"] == model
    assert route["OPENAI_API_URL"] == LOCAL_MP
    slots = json.loads(route["FALLBACK_PROVIDERS"])
    assert all(slot["model"] in MODELS and slot["provider"] == "openai"
               and slot["baseURL"] == LOCAL_MP for slot in slots)


def test_original_jsonl_query_returns_both_verified_source_targets(tmp_path):
    harness = test_pbi.PbiTest()
    question = "where is JSONL parser error conversion and unknown-field handling?"
    result, trace = harness.run_default_semantic_fixture(
        tmp_path,
        question,
        {
            "crates/verbatim-core/src/parser/canonical_jsonl.rs": (
                "let decoded =\n"
                "    permissive::parse_jsonl_value(line, extra, mode).map_err(|error| {\n"
                "        anyhow::Error::new(JsonlDecodeError { line_no, path, source: error })\n"
                "    })?;\n"
            ),
            "crates/verbatim-core/src/parser/canonical_jsonl/permissive.rs": (
                "if self.package && PACKAGE_FIELDS.contains(&key.as_str()) {\n"
                "    map.next_value::<serde::de::IgnoredAny>()?;\n"
                "} else {\n"
                "    extensions.push((extension_name(None, &key)?, map.next_value()?));\n"
                "}\n"
            ),
            # Lexical overlap alone must not admit a fixture setup line as
            # evidence for parser behavior.
            "crates/verbatim-core/src/parser/canonical_jsonl/tests.rs": (
                'let mut f = NamedTempFile::with_suffix(".jsonl").unwrap();\n'
                "let fixture = \"JSONL parser error conversion unknown-field handling\";\n"
            ),
        },
    )

    assert result.returncode == 0, result.stderr
    assert "Coverage: complete" in result.stdout
    assert "Verified source evidence:" in result.stdout
    assert "crates/verbatim-core/src/parser/canonical_jsonl.rs:" in result.stdout
    assert "map_err" in result.stdout
    assert "crates/verbatim-core/src/parser/canonical_jsonl/permissive.rs:" in result.stdout
    assert "extensions.push" in result.stdout
    assert "crates/verbatim-core/src/parser/canonical_jsonl/tests.rs:" not in result.stdout
    assert "NamedTempFile::with_suffix" not in result.stdout
    assert "let fixture =" not in result.stdout
    assert result.stderr == ""
    assert not trace.exists(), "complete source evidence must not need model chat"


def test_semantic_trace_recovers_targets_beyond_bm25_line_windows(tmp_path):
    harness = test_pbi.PbiTest()
    question = "where is JSONL parser error conversion and unknown-field handling?"
    repo = tmp_path / "repo"
    parser = repo / "src/parser/canonical_jsonl.rs"
    permissive = repo / "src/parser/canonical_jsonl/permissive.rs"
    api_decoy = repo / "src/api/unknown_field.rs"
    parser.parent.mkdir(parents=True)
    permissive.parent.mkdir(parents=True)
    api_decoy.parent.mkdir(parents=True)
    parser_lines = ["fn generated_id() {}"] + ["let value = 1;"] * 48
    parser_error_line = len(parser_lines) + 1
    parser_lines += [
        "let decoded = permissive::parse_jsonl_value(line, extra, mode).map_err(|error| {",
        "    anyhow::Error::new(JsonlDecodeError { line_no, path, source: error })",
        "})?;",
    ]
    parser.write_text("\n".join(parser_lines) + "\n")
    permissive_lines = ["struct PermissiveRecord;"] + ["let value = 1;"] * 48
    unknown_field_line = len(permissive_lines) + 1
    permissive_lines += [
        "if self.package && PACKAGE_FIELDS.contains(&key.as_str()) {",
        "    map.next_value::<serde::de::IgnoredAny>()?;",
        "} else {",
        "    extensions.push((extension_name(None, &key)?, map.next_value()?));",
        "}",
    ]
    permissive.write_text("\n".join(permissive_lines) + "\n")
    api_decoy.write_text(
        "#[serde(deny_unknown_fields)]\n"
        "fn reject_unknown_field() {}\n"
    )

    env, trace = harness.fake_environment(tmp_path)
    probe = tmp_path / "probe"
    probe.write_text(
        "#!/usr/bin/env python3\n"
        f"print('File: {parser}, Lines: 1-1')\n"
        f"print('File: {permissive}, Lines: 1-1')\n"
        f"print('File: {api_decoy}, Lines: 1-1')\n"
    )
    probe.chmod(0o755)
    env["PBI_TEST_PROBE"] = str(probe)
    result = harness.run_pbi(
        question,
        env=env,
        cwd=repo,
        binary=harness.fake_pbi(tmp_path, probe),
        timeout=8,
    )

    assert result.returncode == 0, result.stderr
    assert "Coverage: complete" in result.stdout
    error_conversion_line = parser_error_line
    assert f"src/parser/canonical_jsonl.rs:{error_conversion_line}" in result.stdout
    assert f"src/parser/canonical_jsonl/permissive.rs:{unknown_field_line + 3}" in result.stdout
    assert "map_err" in result.stdout
    assert "extensions.push" in result.stdout
    assert "src/api/unknown_field.rs" not in result.stdout
    assert result.stderr == ""
    assert not trace.exists()


@pytest.mark.parametrize("model,base,provider", [
    ("spark", BASE, "openai"),
    ("spark", LOCAL_MP, "openai"),
    ("abliterated-qwen-latest-27b-high", BASE, "openai"),
    (MODELS[0] + "/suffix", BASE, "openai"),
    (MODELS[0], "http://127.0.0.1:8317/v1", "openai"),
    (MODELS[0], "http://localhost:18318/v1", "openai"),
    (MODELS[0], "http://127.0.0.1:18317/v1", "openai"),
    (MODELS[0], BASE + "/", "openai"),
    (MODELS[0], "http://fixture-secret@gb10:18009/v1", "openai"),
    (MODELS[0], BASE, "anthropic"),
])
def test_unapproved_route_fails_closed_before_chat(tmp_path, model, base, provider):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    config = tmp_path / "config.toml"
    config.write_text(f'primary_model = "{model}"\n' + endpoint(model, base, provider))
    env["PBI_CONFIG_FILE"] = str(config)
    env["LOCAL_ROUTER_BASEURL"] = base
    for args in (("--message", "hello"), ("--debug-config",)):
        result = harness.run_pbi(*args, env=env)
        assert result.returncode == 78
        assert "phase=routing category=unapproved-local-route" in result.stderr
        assert result.stdout == ""
        assert "fixture-secret" not in result.stderr
        assert not trace.exists()
    query = harness.run_pbi("where is router admission", env=env)
    assert query.returncode == 78
    assert "phase=routing category=unapproved-local-route" in query.stderr
    assert "abliterated-qwen-latest-27b-none" in query.stderr
    assert "http://gb10:18009/v1" in query.stderr
    assert query.stdout == ""
    assert "fixture-secret" not in query.stdout + query.stderr
    assert not trace.exists()


def test_unapproved_route_allows_verified_named_file_fast_path(tmp_path):
    """A source-verified no-model fast path precedes route admission."""
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    (tmp_path / "README.md").write_text("evidence: route admission\n")
    config = tmp_path / "config.toml"
    config.write_text(
        'primary_model = "abliterated-qwen-latest-27b-none"\n'
        + endpoint("spark", "http://127.0.0.1:8317/v1")
    )
    env["PBI_CONFIG_FILE"] = str(config)
    result = harness.run_pbi("where is README.md?", env=env, cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert result.stdout == "README.md:1\n"
    assert result.stderr == ""
    assert not trace.exists()


@pytest.mark.parametrize("name,value", [
    ("LOCAL_MODEL", "spark"), ("LLM_MODEL", "gpt-6-luna"),
    ("FALLBACK_MODEL", "opencode/deepseek-v4-flash"),
    ("LOCAL_ROUTER_BASEURL", "http://127.0.0.1:8317/v1"),
    ("CLIPROXY_BASE_URL", "http://127.0.0.1:8317/v1"),
])
def test_environment_cannot_admit_unsafe_route(tmp_path, name, value):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    env.update(LOCAL_MODEL=MODELS[0], LOCAL_ROUTER_BASEURL=BASE, FALLBACK_MODEL=MODELS[0])
    if name == "LLM_MODEL":
        env.pop("LOCAL_MODEL")
    env[name] = value
    result = harness.run_pbi("--message", "hello", env=env)
    assert result.returncode == 78
    assert "phase=routing category=unapproved-local-route" in result.stderr
    assert not trace.exists()


def test_hostile_dotenv_does_not_block_no_model_query(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    env.pop("CLIPROXY_API_KEY")
    (tmp_path / ".env").write_text(
        "LOCAL_ROUTER_BASEURL=http://127.0.0.1:8317/v1\n"
        "LOCAL_ROUTER_API_KEY=dotenv-secret\n"
        f"LLM_MODEL={MODELS[0]}\n"
        "FALLBACK_MODEL=spark\n"
    )
    result = harness.run_pbi("search", "--bm25", "PBI_VERSION", env=env, cwd=tmp_path)
    assert result.returncode != 78
    assert "unapproved-local-route" not in result.stderr
    assert "dotenv-secret" not in result.stdout + result.stderr
    assert not trace.exists()


def test_hostile_dotenv_blocks_planner_before_model(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    env.pop("CLIPROXY_API_KEY")
    source = tmp_path / "unrelated.py"
    source.write_text("# unrelated candidate\n" * 7)
    probe = tmp_path / "probe"
    probe.write_text(
        "#!/usr/bin/env python3\n"
        "import sys\n"
        "if '--dry-run' in sys.argv:\n"
        "    print('File: " + str(source) + ", Lines: 5-5')\n"
        "    print('File: " + str(source) + ", Lines: 7-7')\n"
        "else:\n"
        "    print('NONE')\n"
    )
    probe.chmod(0o755)
    env["PBI_TEST_PROBE"] = str(probe)
    (tmp_path / ".env").write_text(
        "LOCAL_ROUTER_BASEURL=http://127.0.0.1:8317/v1\n"
        f"LLM_MODEL={MODELS[0]}\n"
        "FALLBACK_MODEL=spark\n"
    )
    result = harness.run_pbi(
        "where is compression publication and cache key assembly?",
        env=env,
        cwd=tmp_path,
    )
    assert result.returncode == 78
    assert "phase=routing category=unapproved-local-route" in result.stderr
    assert "abliterated-qwen-latest-27b-none" in result.stderr
    assert "http://gb10:18009/v1" in result.stderr
    assert result.stdout == ""
    assert "dotenv-secret" not in result.stdout + result.stderr
    assert not trace.exists()


def test_unapproved_planner_refusal_does_not_emit_unrelated_location(tmp_path):
    """Planner refusal is not a BM25 success, even when Probe has a coarse hit.

    The fast path has nothing to verify here. Probe may run, but the unapproved
    route must be refused before planner/chat and both files stay uncited.
    """
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    relevant = tmp_path / "canonical_jsonl.rs"
    relevant.write_text("fn convert_parser_error() {}\nfn reject_unknown_field() {}\n")
    decoy = tmp_path / "epub_inspect.rs"
    decoy.write_text("fn inspect_epub() {}\n" * 719)
    calls = tmp_path / "probe-calls"
    probe = tmp_path / "probe"
    probe.write_text(
        "#!/usr/bin/env python3\n"
        "import sys\n"
        "open(" + repr(str(calls)) + ", 'a').write(' '.join(sys.argv[1:]) + '\\n')\n"
        "if '--dry-run' in sys.argv:\n"
        "    raise SystemExit(0)\n"
        "print('File: " + str(decoy) + ", Lines: 719-719')\n"
    )
    probe.chmod(0o755)
    env["PBI_TEST_PROBE"] = str(probe)
    config = tmp_path / "config.toml"
    config.write_text(
        'primary_model = "abliterated-qwen-latest-27b-none"\n'
        + endpoint("spark", "http://127.0.0.1:8317/v1")
    )
    env["PBI_CONFIG_FILE"] = str(config)
    query = harness.run_pbi(
        "where is JSONL parser error conversion and unknown-field handling?",
        env=env,
        cwd=tmp_path,
    )
    assert query.returncode == 78, query.stderr
    assert query.stdout == ""
    assert "phase=routing category=unapproved-local-route" in query.stderr
    assert "abliterated-qwen-latest-27b-none" in query.stderr
    assert "http://gb10:18009/v1" in query.stderr
    assert "canonical_jsonl.rs" not in query.stdout + query.stderr
    assert "epub_inspect.rs" not in query.stdout + query.stderr
    assert "fixture-secret" not in query.stdout + query.stderr
    assert not trace.exists()
    assert calls.exists()
    for args in (("--message", "hello"), ("--debug-config",)):
        blocked = harness.run_pbi(*args, env=env, cwd=tmp_path)
        assert blocked.returncode == 78
        assert "phase=routing category=unapproved-local-route" in blocked.stderr
        assert "abliterated-qwen-latest-27b-none" in blocked.stderr
        assert "fixture-secret" not in blocked.stderr
        assert blocked.stdout == ""
    assert not trace.exists()


def test_hostile_dotenv_blocks_chat_before_model(tmp_path):
    harness = test_pbi.PbiTest()
    env, trace = harness.fake_environment(tmp_path)
    env.pop("CLIPROXY_API_KEY")
    (tmp_path / ".env").write_text(
        "LOCAL_ROUTER_BASEURL=http://127.0.0.1:8317/v1\n"
        "LOCAL_ROUTER_API_KEY=dotenv-secret\n"
        f"LLM_MODEL={MODELS[0]}\n"
        "FALLBACK_MODEL=spark\n"
    )
    chat = harness.run_pbi("--message", "hello", env=env, cwd=tmp_path)
    assert chat.returncode == 78
    assert "phase=routing category=unapproved-local-route" in chat.stderr
    assert "dotenv-secret" not in chat.stdout + chat.stderr
    debug = harness.run_pbi("--debug-config", env=env, cwd=tmp_path)
    assert debug.returncode == 78
    assert "phase=routing category=unapproved-local-route" in debug.stderr
    assert debug.stdout == ""
    assert "dotenv-secret" not in debug.stderr
    assert not trace.exists()
