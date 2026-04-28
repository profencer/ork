# Shared helpers for every stage-N script. Sourced (not executed).
#
# Provides:
#   - DEMO_ROOT, REPO_ROOT, LOG_DIR path constants
#   - load_env / save_env_var (persist key=value into demo/.env)
#   - require_cmd <bin> (fail fast with a friendly message)
#   - log_info / log_warn / log_err pretty-printers (no emoji)
#   - mint_jwt <secret> <tenant_id> <subject> <scopes_csv> [exp_secs]
#       echoes a HS256 JWT to stdout. No external deps beyond `openssl` + bash.
#   - banner <title> draws a section header so the demo output is scannable.
#   - wait_for_url <url> [timeout_secs] polls until 2xx/3xx.

set -Eeuo pipefail

DEMO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$DEMO_ROOT/.." && pwd)"
LOG_DIR="$DEMO_ROOT/logs"
ENV_FILE="$DEMO_ROOT/.env"

mkdir -p "$LOG_DIR" "$DEMO_ROOT/data"

# ANSI helpers — degrade gracefully when not a TTY.
if [[ -t 1 ]]; then
  C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'; C_RED=$'\033[31m'
  C_YEL=$'\033[33m'; C_GRN=$'\033[32m'; C_CYAN=$'\033[36m'; C_RST=$'\033[0m'
else
  C_BOLD=""; C_DIM=""; C_RED=""; C_YEL=""; C_GRN=""; C_CYAN=""; C_RST=""
fi

log_info() { printf '%s[info]%s %s\n' "$C_CYAN" "$C_RST" "$*"; }
log_warn() { printf '%s[warn]%s %s\n' "$C_YEL"  "$C_RST" "$*" >&2; }
log_err()  { printf '%s[err ]%s %s\n' "$C_RED"  "$C_RST" "$*" >&2; }

banner() {
  local title="$*"
  printf '\n%s%s== %s ==%s\n\n' "$C_BOLD" "$C_GRN" "$title" "$C_RST"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_err "missing required command: $cmd"
    exit 127
  fi
}

# Persist a single KEY=VALUE pair into demo/.env, replacing any prior entry.
save_env_var() {
  local key="$1" value="$2"
  touch "$ENV_FILE"
  if grep -q "^${key}=" "$ENV_FILE" 2>/dev/null; then
    if [[ "$(uname -s)" == "Darwin" ]]; then
      sed -i '' "s|^${key}=.*|${key}=${value}|" "$ENV_FILE"
    else
      sed -i "s|^${key}=.*|${key}=${value}|" "$ENV_FILE"
    fi
  else
    printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
  fi
}

# Source demo/.env into the current shell. Stage scripts call this at the top
# so the values stage 0 wrote (TENANT_ID, JWT, BASE_URL, ...) become available.
load_env() {
  if [[ -f "$ENV_FILE" ]]; then
    set -a; source "$ENV_FILE"; set +a
  fi
}

# Base64-URL encode stdin (no padding, no newlines).
b64url() {
  openssl base64 -A | tr -d '=' | tr '/+' '_-'
}

# mint_jwt <secret> <tenant_id> <subject> <scopes_csv> [exp_secs=86400]
#
# Produces an HS256-signed JWT on stdout matching the shape ork's
# auth_middleware decodes (sub, tenant_id, scopes, exp).
mint_jwt() {
  local secret="$1" tenant_id="$2" sub="$3" scopes_csv="$4" exp_secs="${5:-86400}"
  local now exp scopes_json header payload h p sig
  now=$(date +%s)
  exp=$(( now + exp_secs ))
  if [[ -z "$scopes_csv" ]]; then
    scopes_json='[]'
  else
    scopes_json="[\"$(printf '%s' "$scopes_csv" | sed 's/,/","/g')\"]"
  fi
  header='{"alg":"HS256","typ":"JWT"}'
  payload="{\"sub\":\"${sub}\",\"tenant_id\":\"${tenant_id}\",\"scopes\":${scopes_json},\"exp\":${exp}}"
  h=$(printf '%s' "$header" | b64url)
  p=$(printf '%s' "$payload" | b64url)
  sig=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -hmac "$secret" -binary | b64url)
  printf '%s.%s.%s' "$h" "$p" "$sig"
}

# Decode the header of a JWS (compact form) without verifying. Used by
# stage-7 to pretty-print the signature header that lands at the receiver.
jws_header() {
  local jws="$1"
  local header_b64="${jws%%.*}"
  local pad=$(( (4 - ${#header_b64} % 4) % 4 ))
  while (( pad > 0 )); do header_b64+="="; pad=$(( pad - 1 )); done
  printf '%s' "$header_b64" | tr '_-' '/+' | openssl base64 -d -A
}

# Block until a URL responds with 2xx/3xx, or the timeout expires.
wait_for_url() {
  local url="$1" timeout="${2:-30}"
  local start
  start=$(date +%s)
  while true; do
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$url" || echo 000)
    if [[ "$code" =~ ^[23] ]]; then
      return 0
    fi
    if (( $(date +%s) - start > timeout )); then
      log_err "timed out waiting for $url (last code=$code)"
      return 1
    fi
    sleep 0.5
  done
}

# Run psql inside the demo Postgres container so the host doesn't need a
# `psql` binary. Stdin is forwarded so callers can `cat file.sql | demo_psql`.
demo_psql() {
  docker compose -f "$DEMO_ROOT/docker-compose.yml" exec -T postgres \
    psql -U ork -d ork_demo -v ON_ERROR_STOP=1 "$@"
}

# Apply every migration under repo/migrations/ to the demo Postgres in
# filename order (alphanumeric, matches the `001_`, `002_` prefix scheme).
apply_migrations() {
  local migration
  for migration in "$REPO_ROOT"/migrations/*.sql; do
    log_info "applying $(basename "$migration")"
    demo_psql -f - < "$migration" >/dev/null
  done
}

# Kill a process by PID file if the file exists and the PID is alive.
kill_pidfile() {
  local pidfile="$1" name="${2:-process}"
  if [[ -f "$pidfile" ]]; then
    local pid
    pid=$(cat "$pidfile" 2>/dev/null || echo "")
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      log_info "stopping $name (pid $pid)"
      kill "$pid" 2>/dev/null || true
      sleep 0.3
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$pidfile"
  fi
}

# Build + boot the demo/peer-agent on $PEER_ADDR (default 127.0.0.1:8090).
# Sets PEER_ADDR / PEER_CARD_URL / PEER_LOG / PEER_PID_FILE in the caller's
# scope. Idempotent: kills any prior instance via its PID file first.
boot_peer_agent() {
  PEER_ADDR="${PEER_ADDR:-127.0.0.1:8090}"
  PEER_CARD_URL="http://${PEER_ADDR}/.well-known/agent-card.json"
  PEER_LOG="$LOG_DIR/peer-agent.log"
  PEER_PID_FILE="$DEMO_ROOT/.peer-agent.pid"
  local peer_dir="$DEMO_ROOT/peer-agent"

  if curl -s -o /dev/null --max-time 1 "$PEER_CARD_URL"; then
    log_info "peer-agent already responding on $PEER_ADDR — re-using it"
    return 0
  fi

  log_info "building demo/peer-agent (this is a no-op after the first run)"
  ( cd "$peer_dir" && cargo build -q )

  kill_pidfile "$PEER_PID_FILE" "previous peer-agent"

  log_info "launching peer-agent on $PEER_ADDR (logs -> $PEER_LOG)"
  : > "$PEER_LOG"
  (
    cd "$peer_dir"
    nohup env "PEER_ADDR=$PEER_ADDR" "RUST_LOG=info" \
      cargo run -q -- --addr "$PEER_ADDR" \
      >> "$PEER_LOG" 2>&1 &
    echo "$!" > "$PEER_PID_FILE"
  )
  log_info "peer-agent pid=$(cat "$PEER_PID_FILE")"

  if ! wait_for_url "$PEER_CARD_URL" 30; then
    log_err "peer-agent never published its card. Tail of $PEER_LOG:"
    tail -n 40 "$PEER_LOG" >&2 || true
    return 1
  fi
}

# LangGraph A2A peer on $LG_ADDR (default 127.0.0.1:8092). Sets
# LG_ADDR / LG_CARD_URL / LG_LOG / LG_PID_FILE; idempotent; may set LG_SKIPPED=1.
boot_langgraph_agent() {
  LG_SKIPPED=0
  LG_FOREIGN_PEER=0
  LG_ADDR="${LG_ADDR:-127.0.0.1:8092}"
  LG_CARD_URL="http://${LG_ADDR}/.well-known/agent-card.json"
  LG_LOG="$LOG_DIR/langgraph-agent.log"
  LG_PID_FILE="$DEMO_ROOT/.langgraph-agent.pid"
  local lg_dir="$DEMO_ROOT/langgraph-agent"

  # Only treat :8092 as "ours" if the demo PID file points at a live process.
  # Otherwise another LangGraph (or stale manual run) may answer the card URL
  # while nothing appends to demo/logs/langgraph-agent.log — stage 9 traces break.
  if curl -s -o /dev/null --max-time 1 "$LG_CARD_URL"; then
    if [[ -f "$LG_PID_FILE" ]] && kill -0 "$(cat "$LG_PID_FILE")" 2>/dev/null; then
      log_info "langgraph-agent already responding on $LG_ADDR — re-using demo-managed instance (logs -> $LG_LOG)"
      return 0
    fi
    log_warn "something already responds on $LG_CARD_URL but it is not this demo's langgraph-agent (expected live PID in $LG_PID_FILE). Stage 9 will not see [ask_ork] / trace lines in $LG_LOG. Stop the process on $LG_ADDR and run \`make -C demo demo-stage-0\`, or start the peer only via stage 0."
    LG_FOREIGN_PEER=1
    return 0
  fi

  if ! command -v python3 >/dev/null 2>&1; then
    log_warn "skipping langgraph-agent (python3 not found) — stage 9 will no-op"
    LG_SKIPPED=1
    return 0
  fi
  if ! python3 -c 'import sys; assert sys.version_info >= (3, 12)' >/dev/null 2>&1; then
    log_warn "skipping langgraph-agent (need Python >= 3.12) — stage 9 will no-op"
    LG_SKIPPED=1
    return 0
  fi

  kill_pidfile "$LG_PID_FILE" "previous langgraph-agent"

  local py="python3"
  local run_m="demo_langgraph_agent"
  # editable install into .venv (first run runs pip)
  if [[ -x "$lg_dir/.venv/bin/python" ]]; then
    py="$lg_dir/.venv/bin/python"
  else
    log_info "creating demo/langgraph-agent/.venv and pip install -e (one-time)"
    ( cd "$lg_dir" && python3 -m venv .venv && .venv/bin/pip install -q -U pip && .venv/bin/pip install -q -e ".[dev]" ) \
      || { log_warn "langgraph-agent venv install failed — skipping (network or deps); stage 9 will no-op"; LG_SKIPPED=1; return 0; }
    py="$lg_dir/.venv/bin/python"
  fi

  # Minimax: ChatOpenAI wants bare key; demo MINIMAX_API_KEY is often "Bearer sk-…"
  local oai_key oai_url
  oai_url="${OPENAI_BASE_URL:-https://api.minimax.io/v1}"
  oai_key="${OPENAI_API_KEY:-}"
  if [[ -z "$oai_key" && -n "${MINIMAX_API_KEY:-}" ]]; then
    t="${MINIMAX_API_KEY#Bearer}"
    t="${t# }"
    oai_key="${t//[[:space:]]/}"
  fi

  : > "$LG_LOG"
  (
    cd "$lg_dir"
    nohup env \
      "DEMO_ROOT=$DEMO_ROOT" \
      "RUST_LOG=info" \
      "LOG_LEVEL=INFO" \
      "ORK_BASE_URL=${ORK_BASE_URL:-${BASE_URL:-http://127.0.0.1:8080}}" \
      "ORK_JWT=${ORK_JWT:-${JWT:-}}" \
      "ORK_TENANT_ID=${ORK_TENANT_ID:-${TENANT_ID:-}}" \
      "MINIMAX_API_KEY=${MINIMAX_API_KEY:-}" \
      "OPENAI_BASE_URL=$oai_url" \
      "OPENAI_API_KEY=$oai_key" \
      "OPENAI_MODEL=${OPENAI_MODEL:-MiniMax-M2.7}" \
      "$py" -m "$run_m" --addr "$LG_ADDR" \
      >> "$LG_LOG" 2>&1 &
    echo "$!" > "$LG_PID_FILE"
  )
  log_info "langgraph-agent pid=$(cat "$LG_PID_FILE") (logs -> $LG_LOG)"

  if ! wait_for_url "$LG_CARD_URL" 60; then
    log_warn "langgraph-agent never published its card — skipping. Tail of $LG_LOG:"
    tail -n 20 "$LG_LOG" >&2 || true
    kill_pidfile "$LG_PID_FILE" "failed langgraph-agent"
    LG_SKIPPED=1
    return 0
  fi
}

# Convert a YAML file to JSON via the best available tool, in order:
#   1. mikefarah's `yq` (the Go one) or kislyuk/yq (the Python wrapper).
#   2. `python3` + PyYAML.
#   3. A sibling `.json` snapshot (same basename, `.yaml`/`.yml` swapped).
#   4. An explicit fallback path passed as the optional 2nd argument.
#
# (4) lets a caller bundle a JSON snapshot under `demo/` for a YAML that
# lives outside `demo/` (e.g. `workflow-templates/change-plan.yaml`) so the
# demo doesn't need write access to the workspace asset directory.
#
# Echoes the JSON to stdout. Exits 127 if no source is usable.
yaml_to_json() {
  local file="$1"
  local explicit_fallback="${2:-}"
  if command -v yq >/dev/null 2>&1; then
    if yq --version 2>/dev/null | grep -qi 'mikefarah'; then
      yq -o json '.' "$file"
      return
    fi
    yq . "$file"
    return
  fi
  if command -v python3 >/dev/null 2>&1 && python3 -c 'import yaml' >/dev/null 2>&1; then
    python3 - "$file" <<'PY'
import json, sys, yaml
with open(sys.argv[1]) as f:
    print(json.dumps(yaml.safe_load(f)))
PY
    return
  fi
  local sibling="${file%.*}.json"
  if [[ -f "$sibling" ]]; then
    log_info "yq/PyYAML unavailable — using bundled JSON snapshot $(basename "$sibling")" >&2
    cat "$sibling"
    return
  fi
  if [[ -n "$explicit_fallback" && -f "$explicit_fallback" ]]; then
    log_info "yq/PyYAML unavailable — using bundled JSON snapshot $explicit_fallback" >&2
    cat "$explicit_fallback"
    return
  fi
  log_err "need either 'yq', 'python3 + PyYAML', or a JSON snapshot next to $(basename "$file") to convert YAML to JSON."
  exit 127
}
