#!/usr/bin/env bash
# pbi: non-interactive Probe Chat wrapper for the local OpenAI-compatible endpoint.
set -euo pipefail

readonly PBI_VERSION="0.1.0"
readonly DEFAULT_BASE_URL="http://localhost:8317/v1"
readonly DEFAULT_PRIMARY_MODEL="qwen3.6-27b-decensor-by-aeon"
readonly DEFAULT_FALLBACK_MODEL="opencode/deepseek-v4-flash"
readonly DEFAULT_REQUEST_TIMEOUT_MS="1700000"
readonly DEFAULT_OPERATION_TIMEOUT_MS="8500000"
readonly DEFAULT_SEARCH_TIMEOUT_SECONDS="540"
readonly DEFAULT_SEARCH_MAX_RESULTS="8"

usage() {
  printf '%s\n' "pbi ${PBI_VERSION} — Probe Chat wrapper"
  printf '%s\n' "Usage: pbi <question...> [--json]"
  printf '%s\n' "       pbi search [--bm25] <query>"
  printf '%s\n' "       pbi --message <question> [probe-chat options]"
  printf '%s\n' "       pbi --debug-config"
  printf '%s\n' "Search defaults to local-model ranking; use --bm25 for no-LLM Probe output."
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

compact_search_locations() {
  local line file location suffix
  while IFS= read -r line; do
    if [[ "$line" =~ ^File:[[:space:]]+(.+)$ ]]; then
      file="${BASH_REMATCH[1]}"
      file="${file%%, Lines:*}"
      file="$(realpath --relative-to="$PWD" -- "$file" 2>/dev/null || true)"
      if [[ "$file" != /* && "$file" != ../* && -f "$file" ]]; then
        printf '%s:1\n' "$file"
      fi
    elif [[ "$line" =~ ([[:alnum:]_./-]+:([[:alnum:]_~-]+|[[:digit:]]+)) ]]; then
      location="${BASH_REMATCH[1]}"
      if [[ "$location" == /* ]]; then
        file="$(realpath --relative-to="$PWD" -- "${location%:*}" 2>/dev/null || true)"
        suffix="${location##*:}"
        if [[ "$file" != /* && "$file" != ../* && -f "$file" ]]; then
          printf '%s:%s\n' "$file" "$suffix"
        fi
      else
        printf '%s\n' "$location"
      fi
    fi
  done <<<"$1"
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
    printf '%s\n' 'pbi: question is required; interactive mode is disabled' >&2
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
search_uses_local_model=false
search_fallback_locations=""

configure_local_routing() {
  api_key="${CLIPROXY_API_KEY:-${OPENAI_API_KEY:-}}"
  if [[ -z "$api_key" ]]; then
    printf '%s\n' 'pbi: set CLIPROXY_API_KEY or OPENAI_API_KEY in the environment or ~/.pbi/config' >&2
    return 78
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
  export ALLOWED_FOLDERS="$PWD"
}

case "${1:-}" in
  search)
    shift
    for argument in "$@"; do
      if [[ "$argument" == "--help" || "$argument" == "-h" ]]; then
        exec "$(resolve_probe)" search "$@"
      fi
    done
    search_options=()
    search_pattern_parts=()
    search_timeout_set=false
    search_max_results_set=false
    search_bm25=false
    while (($#)); do
      argument="$1"
      shift
      case "$argument" in
        --bm25)
          search_bm25=true
          ;;
        --)
          search_pattern_parts+=("$@")
          break
          ;;
        --timeout|--timeout=*)
          search_timeout_set=true
          search_options+=("$argument")
          if [[ "$argument" == "--timeout" && $# -gt 0 ]]; then
            search_options+=("$1")
            shift
          fi
          ;;
        --max-results|--max-results=*)
          search_max_results_set=true
          search_options+=("$argument")
          if [[ "$argument" == "--max-results" && $# -gt 0 ]]; then
            search_options+=("$1")
            shift
          fi
          ;;
        --reranker|-r)
          if (($#)); then
            shift
          fi
          ;;
        --reranker=*)
          ;;
        --ignore|-i|--language|-l|--max-bytes|--max-tokens|--merge-threshold|--format|-o|--session|--question)
          search_options+=("$argument")
          if (($#)); then
            search_options+=("$1")
            shift
          fi
          ;;
        -*)
          search_options+=("$argument")
          ;;
        *)
          search_pattern_parts+=("$argument")
          ;;
      esac
    done
    if [[ "$search_timeout_set" == false ]]; then
      search_options=(--timeout "$DEFAULT_SEARCH_TIMEOUT_SECONDS" "${search_options[@]}")
    fi
    if [[ "$search_max_results_set" == false ]]; then
      search_options+=(--max-results "$DEFAULT_SEARCH_MAX_RESULTS")
    fi
    if [[ "$search_bm25" == true ]]; then
      exec "$(resolve_probe)" search --reranker bm25 "${search_options[@]}" -- "${search_pattern_parts[*]}"
    fi
    search_options+=(--ignore drafts)
    configure_local_routing
    search_uses_local_model=true
    if ! candidates="$("$(resolve_probe)" search "${search_options[@]}" --reranker bm25 --format plain --dry-run -- "${search_pattern_parts[*]}" 2>&1)"; then
      printf '%s\n' "$candidates" >&2
      exit 1
    fi
    candidates="$(printf '%s\n' "$candidates" | grep -Ev "^BERT reranker .* is not available\.$|^Falling back to BM25 ranking\.\.\.$" || true)"
    search_fallback_locations="$(compact_search_locations "$candidates")"
    set -- --message "Use Probe BM25 candidates to find ${search_pattern_parts[*]}. Return only the best matching path:symbol or path:line locations; no narration."$'\n\n'"$candidates"
    ;;
esac

if [[ "$1" == "--message" ]]; then
  shift
  if (($# == 0)); then
    printf '%s\n' 'pbi: question is required; interactive mode is disabled' >&2
    exit 2
  fi
  chat_args=(--message "$1")
  shift
  chat_args+=("$@")
else
  message_parts=()
  chat_args=()
  for argument in "$@"; do
    if [[ "$argument" == "--json" ]]; then
      chat_args+=("$argument")
    else
      message_parts+=("$argument")
    fi
  done
  if ((${#message_parts[@]} == 0)); then
    printf '%s\n' 'pbi: question is required; interactive mode is disabled' >&2
    exit 2
  fi
  chat_args=(--message "${message_parts[*]}" "${chat_args[@]}")
fi

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
  printf '%s\n' "search_timeout_seconds=$DEFAULT_SEARCH_TIMEOUT_SECONDS"
  printf '%s\n' 'search_default=local_model'
  printf '%s\n' 'search_bm25_opt_in=--bm25'
  printf '%s\n' 'api_key=[REDACTED]'
  exit 0
fi

configure_local_routing

if [[ "$search_uses_local_model" == true ]]; then
  if output="$("$agent_command" --force-provider openai --model-name "$primary_model" "${chat_args[@]}" 2>&1)"; then
    status=0
  else
    status=$?
  fi
elif output="$("$agent_command" --force-provider openai --model-name "$primary_model" "${chat_args[@]}")"; then
  status=0
else
  status=$?
fi
if ((status != 0)); then
  printf '%s\n' "$output"
  exit "$status"
fi
if probe_reported_error "$output"; then
  printf '%s\n' "$output"
  printf '%s\n' 'pbi: probe-chat reported an API error' >&2
  exit 1
fi
if [[ "$search_uses_local_model" == true ]]; then
  output="$(compact_search_locations "$output")"
  if [[ -z "$output" ]]; then
    output="$search_fallback_locations"
  fi
  if [[ -z "$output" ]]; then
    printf '%s\n' 'pbi: local search returned no compact locations' >&2
    exit 1
  fi
fi
printf '%s\n' "$output"
