#!/usr/bin/env bash
# pbi: non-interactive Probe Chat wrapper for the local OpenAI-compatible endpoint.
set -euo pipefail

readonly PBI_VERSION="0.1.0"
readonly DEFAULT_BASE_URL="http://localhost:8317/v1"
readonly DEFAULT_PRIMARY_MODEL="qwen3.6-27b-decensor-by-aeon"
readonly DEFAULT_FALLBACK_MODEL="opencode/deepseek-v4-flash"
readonly DEFAULT_REQUEST_TIMEOUT_MS="1700000"
readonly DEFAULT_OPERATION_TIMEOUT_MS="8500000"

usage() {
  printf '%s\n' "pbi ${PBI_VERSION} — Probe Chat wrapper"
  printf '%s\n' "Usage: pbi --message <question> [probe-chat options]"
  printf '%s\n' "       pbi --debug-config"
  printf '%s\n' "Routes OpenAI-compatible requests to localhost:8317/v1 with primary retries then fallback."
}

resolve_probe() {
  local probe_path
  probe_path="/usr/local/share/mise/shims/probe"
  if [[ ! -x "$probe_path" ]]; then
    printf '%s\n' 'pbi: mise-installed probe is unavailable' >&2
    return 127
  fi
  printf '%s' "$probe_path"
}

probe_reported_error() {
  local output="$1"
  if printf '%s' "$output" | node -e '
let input = "";
process.stdin.on("data", chunk => { input += chunk; });
process.stdin.on("end", () => {
  try {
    const response = JSON.parse(input);
    process.exit(response && (response.error || response.errors || response.status === "error") ? 0 : 1);
  } catch {
    process.exit(1);
  }
});'; then
    return 0
  fi
  grep -Eiq 'invalid_request|codex[[:space:]-]*fallback|fallback[[:space:]-]*codex|model[_[:space:]-]*(not[_[:space:]-]*found|missing|unavailable)' <<<"$output"
}

case "${1:-}" in
  --help|-h)
    usage
    exit 0
    ;;
  --version|-V)
    printf 'pbi %s\n' "$PBI_VERSION"
    exit 0
    ;;
  '')
    printf '%s\n' 'pbi: --message is required; interactive mode is disabled' >&2
    exit 2
    ;;
esac

config_file="${PBI_CONFIG_FILE:-$HOME/.pbi/config}"
if [[ -r "$config_file" ]]; then
  # shellcheck disable=SC1090
  source "$config_file"
fi

base_url="${CLIPROXY_BASE_URL:-$DEFAULT_BASE_URL}"
primary_model="${LOCAL_MODEL:-$DEFAULT_PRIMARY_MODEL}"
fallback_model="${FALLBACK_MODEL:-$DEFAULT_FALLBACK_MODEL}"
request_timeout="${REQUEST_TIMEOUT_MS:-$DEFAULT_REQUEST_TIMEOUT_MS}"
operation_timeout="${MAX_OPERATION_TIMEOUT_MS:-$DEFAULT_OPERATION_TIMEOUT_MS}"
max_retries="3"
probe_path="$(resolve_probe)"
agent_command="$(command -v probe-chat || true)"
if [[ -z "$agent_command" ]]; then
  printf '%s\n' 'pbi: probe-chat is unavailable on PATH' >&2
  exit 127
fi

if [[ "${1:-}" == "--debug-config" ]]; then
  printf '%s\n' "probe_binary=$probe_path"
  printf '%s\n' 'provider=openai'
  printf '%s\n' "primary_model=$primary_model"
  printf '%s\n' "fallback_model=$fallback_model"
  printf '%s\n' "base_url=$base_url"
  printf '%s\n' "request_timeout_ms=$request_timeout"
  printf '%s\n' "max_operation_timeout_ms=$operation_timeout"
  printf '%s\n' "max_retries=$max_retries"
  printf '%s\n' 'api_key=[REDACTED]'
  exit 0
fi

api_key="${CLIPROXY_API_KEY:-${OPENAI_API_KEY:-}}"
if [[ -z "$api_key" ]]; then
  printf '%s\n' 'pbi: set CLIPROXY_API_KEY or OPENAI_API_KEY in the environment or ~/.pbi/config' >&2
  exit 78
fi

fallback_providers="$(
  PBI_BASE_URL="$base_url" PBI_API_KEY="$api_key" PBI_PRIMARY_MODEL="$primary_model" \
    PBI_FALLBACK_MODEL="$fallback_model" node -e '
const {PBI_BASE_URL, PBI_API_KEY, PBI_PRIMARY_MODEL, PBI_FALLBACK_MODEL} = process.env;
process.stdout.write(JSON.stringify([
  {provider: "openai", apiKey: PBI_API_KEY, baseURL: PBI_BASE_URL, model: PBI_PRIMARY_MODEL, maxRetries: 3},
  {provider: "openai", apiKey: PBI_API_KEY, baseURL: PBI_BASE_URL, model: PBI_FALLBACK_MODEL, maxRetries: 0}
]));'
)"

export PROBE_BINARY_PATH="$probe_path"
export FORCE_PROVIDER="openai"
export MODEL_NAME="$primary_model"
export OPENAI_API_KEY="$api_key"
export OPENAI_API_URL="$base_url"
export LLM_BASE_URL="$base_url"
export REQUEST_TIMEOUT="$request_timeout"
export MAX_OPERATION_TIMEOUT="$operation_timeout"
export MAX_RETRIES="$max_retries"
export FALLBACK_PROVIDERS="$fallback_providers"

if output="$("$agent_command" --force-provider openai --model-name "$primary_model" "$@")"; then
  status=0
else
  status=$?
fi
printf '%s\n' "$output"
if ((status != 0)); then
  exit "$status"
fi
if probe_reported_error "$output"; then
  printf '%s\n' 'pbi: probe-chat reported an API error' >&2
  exit 1
fi
