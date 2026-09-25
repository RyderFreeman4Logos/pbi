"""GB10 route admission is a closed model/base contract, not file order."""
import json

import pytest

import test_pbi

BASE = "http://gb10:18009/v1"
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


@pytest.mark.parametrize("model,base,provider", [
    ("spark", BASE, "openai"),
    ("abliterated-qwen-latest-27b-high", BASE, "openai"),
    (MODELS[0] + "/suffix", BASE, "openai"),
    (MODELS[0], "http://127.0.0.1:8317/v1", "openai"),
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
    for args in (("--message", "hello"), ("where is router admission",), ("--debug-config",)):
        result = harness.run_pbi(*args, env=env)
        assert result.returncode == 78
        assert "phase=routing category=unapproved-local-route" in result.stderr
        assert result.stdout == ""
        assert "fixture-secret" not in result.stderr
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
