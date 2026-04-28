#!/usr/bin/env bash
# Stage 0 — Bootstrap.
#
# Brings the demo from a cold laptop to "ork-api is serving on :8080":
#
#   1. compose up Postgres + Redis (waits for healthchecks)
#   2. apply every `migrations/*.sql` against the demo DB
#   3. mint a bootstrap JWT, POST /api/tenants to seed the demo tenant
#   4. mint a per-tenant JWT and persist (TENANT_ID, JWT, BASE_URL, ...)
#      into demo/.env so subsequent stage scripts can reuse them
#   5. nohup-launch ork-api in the background; PID lives in demo/.ork-api.pid
#      and stdout/stderr stream to demo/logs/ork-api.log
#
# Re-running this script is safe: existing containers are reused, the tenant
# is created idempotently (slug uniqueness in the DB short-circuits the
# duplicate), and the previous ork-api PID is killed before relaunching.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

banner "Stage 0 — Bootstrap"

require_cmd docker
require_cmd curl
require_cmd jq
require_cmd openssl
require_cmd cargo

# 1. Compose --------------------------------------------------------------
log_info "starting Postgres + Redis via docker compose"
docker compose -f "$DEMO_ROOT/docker-compose.yml" up -d --wait
log_info "postgres + redis are healthy"

# 2. Migrations -----------------------------------------------------------
# Drop+recreate the public schema first so re-runs of stage 0 don't fail
# with "relation already exists" errors. The demo is a throwaway DB, so
# wiping its contents is fine.
log_info "resetting demo Postgres schema (drop + recreate public)"
demo_psql -c 'DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public; GRANT ALL ON SCHEMA public TO ork; GRANT ALL ON SCHEMA public TO public;' >/dev/null

log_info "applying migrations against ork_demo"
apply_migrations
log_info "migrations applied"

# 3. peer-agent boot (before ork-api so the static registry can resolve it) -
# ADR 0007 / config_default.toml registers `vendor-planner` at
# http://127.0.0.1:8090/. ork-api fetches the peer's AgentCard at boot
# (`load_static_remote_agents`); the background `spawn_card_refresh` only
# re-ticks every `card_refresh_interval` (default 5 minutes), so if we let
# ork-api boot first the registry stays empty for the entire demo. Boot
# the peer here, before ork-api, so the boot-time loader succeeds.
boot_peer_agent

# 3.4 LangGraph A2A peer (ADR-0007) — :8092, before ork-api static remote_agent card fetch
# (same reason as boot_peer_agent). `ask_ork` back-calls ork; creds from demo/.env at runtime
# (DEMO_ROOT) once stage-0 has written the file.
boot_langgraph_agent

# 3.5 Web UI (ADR-0017) — optional Vite on :5173, before ork, so the API can set
# WEBUI_DEV_PROXY.  Disable with DEMO_WEBUI=0.  Needs `pnpm` + `client/webui/frontend`.
BASE_URL="http://127.0.0.1:8080"
WEBUI_VITE_PORT="${WEBUI_VITE_PORT:-5173}"
WEBUI_VITE_LOG="$LOG_DIR/webui-vite.log"
VITE_PID_FILE="$DEMO_ROOT/.webui-vite.pid"
DEMO_WEBUI="${DEMO_WEBUI:-1}"
ORK_PUBLIC_EXTRAS=("ORK_A2A_PUBLIC_BASE=$BASE_URL")
if [[ "$DEMO_WEBUI" == "0" ]]; then
  kill_pidfile "$VITE_PID_FILE" "webui vite (DEMO_WEBUI=0)"
fi
if [[ "$DEMO_WEBUI" != "0" ]] && command -v pnpm >/dev/null 2>&1 && [[ -f "$REPO_ROOT/client/webui/frontend/package.json" ]]; then
  kill_pidfile "$VITE_PID_FILE" "previous webui vite"
  log_info "Web UI: installing client/webui/frontend (pnpm) — first run can take a minute"
  ( cd "$REPO_ROOT/client/webui/frontend" && ( pnpm install --frozen-lockfile 2>/dev/null || pnpm install ) ) \
    || { log_err "pnpm install failed in client/webui/frontend — set DEMO_WEBUI=0 to skip the Web UI"; exit 1; }
  : > "$WEBUI_VITE_LOG"
  (
    cd "$REPO_ROOT/client/webui/frontend"
    # `--host 127.0.0.1` so the server binds IPv4; default `localhost` can be ::1 only,
    # which makes `http://127.0.0.1:$port/` (and ork's WEBUI_DEV_PROXY) time out.
    nohup pnpm run dev -- --host 127.0.0.1 --port "$WEBUI_VITE_PORT" --strictPort \
      >> "$WEBUI_VITE_LOG" 2>&1 &
    echo "$!" > "$VITE_PID_FILE"
  )
  log_info "Vite for Web UI (pid $(cat "$VITE_PID_FILE"), log $WEBUI_VITE_LOG)"
  if ! wait_for_url "http://127.0.0.1:${WEBUI_VITE_PORT}/" 60; then
    log_err "Vite did not start — see $WEBUI_VITE_LOG"
    tail -n 50 "$WEBUI_VITE_LOG" >&2 || true
    exit 1
  fi
  ORK_PUBLIC_EXTRAS+=("WEBUI_DEV_PROXY=http://127.0.0.1:${WEBUI_VITE_PORT}")
  log_info "Web UI: open $BASE_URL/ in a browser, paste JWT from demo/.env (same origin API)"
else
  if [[ "$DEMO_WEBUI" != "0" ]]; then
    log_warn "skipping Web UI Vite (install pnpm and run from repo with client/webui/frontend, or set DEMO_WEBUI=0). /webui/api/* still works; / may 503 without WEBUI_DEV_PROXY."
  fi
  ORK_PUBLIC_EXTRAS=("ORK_A2A_PUBLIC_BASE=$BASE_URL")
fi

# 4. ork-api boot ---------------------------------------------------------
PID_FILE="$DEMO_ROOT/.ork-api.pid"
LOG_FILE="$LOG_DIR/ork-api.log"
JWT_SECRET="ork-demo-secret-change-me"

# Kill any prior ork-api this script started (lets you re-run stage 0 freely).
kill_pidfile "$PID_FILE" "previous ork-api"

# If something else is already on :8080, stop here with a clear error so
# stage 1+ don't talk to a foreign service.
if curl -s -o /dev/null --max-time 1 "$BASE_URL/health"; then
  log_warn "something is already responding on $BASE_URL/health — re-using it (not starting a new ork-server)"
  if [[ -f "$VITE_PID_FILE" ]]; then
    log_warn "Vite is running; if $BASE_URL/ does not show the Web UI, stop whatever holds :8080 and re-run stage 0 so ork starts with WEBUI_DEV_PROXY"
  fi
else
  log_info "launching ork-api in the background (logs -> $LOG_FILE)"

  # `cargo run` would block this script until cargo finishes. Build first
  # (foreground), then start the binary in the background. Note: the package
  # is `ork-api` but the binary is named `ork-server` (see crates/ork-api/Cargo.toml
  # `[[bin]] name = "ork-server"`).
  log_info "compiling ork-api (this is a no-op after the first run)"
  ( cd "$REPO_ROOT" && cargo build -q -p ork-api --bin ork-server )
  ORK_API_BIN="$REPO_ROOT/target/debug/ork-server"
  if [[ ! -x "$ORK_API_BIN" ]]; then
    log_err "expected built binary at $ORK_API_BIN — did the build fail?"
    log_err "tip: run \`cargo build -p ork-api --bin ork-server\` manually to inspect the error."
    exit 1
  fi

  # Env var precedence (config crate): ORK__* wins over the toml file. The
  # config crate looks for `config/default.toml` relative to CWD, so we
  # `cd` into `demo/` first to pick up `demo/config/default.toml`.
  : > "$LOG_FILE"
  ENV_OVERRIDES=(
    "ORK__SERVER__HOST=127.0.0.1"
    "ORK__SERVER__PORT=8080"
    "ORK__AUTH__JWT_SECRET=$JWT_SECRET"
    "ORK__DATABASE__URL=postgres://ork:ork@127.0.0.1:5433/ork_demo"
    "ORK__REDIS__URL=redis://127.0.0.1:6380/"
    "RUST_LOG=${RUST_LOG:-info,ork_api=info,ork_mcp=info,ork_integrations=info,ork_push=info}"
    # Bracket assistant text deltas in ork-api.log as `LLM output (researcher)` blocks (stage 9).
    "ORK_PRINT_LLM_OUTPUT=1"
  )
  if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
    ENV_OVERRIDES+=("MINIMAX_API_KEY=$MINIMAX_API_KEY")
  fi
  (
    cd "$DEMO_ROOT"
    nohup env "${ENV_OVERRIDES[@]}" "${ORK_PUBLIC_EXTRAS[@]}" "$ORK_API_BIN" >> "$LOG_FILE" 2>&1 &
    echo "$!" > "$PID_FILE"
  )
  log_info "ork-api pid=$(cat "$PID_FILE")"

  log_info "waiting for $BASE_URL/health"
  if ! wait_for_url "$BASE_URL/health" 60; then
    log_err "ork-api never became healthy — see $LOG_FILE"
    tail -n 80 "$LOG_FILE" >&2 || true
    exit 1
  fi
fi

# 4. Seed the demo tenant -------------------------------------------------
TENANT_SLUG="adeo-ork-demo"
TENANT_NAME="Adeo Ork Demo"

# Bootstrap JWT: tenant-agnostic, scopes include `tenant:admin` so we can
# POST /api/tenants. The middleware just needs a parseable UUID; nil works.
BOOT_JWT=$(mint_jwt "$JWT_SECRET" "00000000-0000-0000-0000-000000000000" \
                    "demo-bootstrap" "tenant:admin" 600)

log_info "creating tenant '$TENANT_SLUG'"
TENANT_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $BOOT_JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/tenants" \
  -d "$(jq -nc --arg name "$TENANT_NAME" --arg slug "$TENANT_SLUG" \
        '{name:$name, slug:$slug}')")
TENANT_BODY=$(printf '%s' "$TENANT_RESP" | sed '$d')
TENANT_CODE=$(printf '%s' "$TENANT_RESP" | tail -n1)

if [[ "$TENANT_CODE" == "201" ]]; then
  TENANT_ID=$(printf '%s' "$TENANT_BODY" | jq -r '.id')
  log_info "created tenant id=$TENANT_ID"
elif printf '%s' "$TENANT_BODY" | grep -q -i "already exists\|duplicate\|unique"; then
  log_info "tenant already exists, looking it up"
  TENANT_ID=$(curl -sS -H "Authorization: Bearer $BOOT_JWT" \
    "$BASE_URL/api/tenants" | jq -r --arg slug "$TENANT_SLUG" \
    '.[] | select(.slug == $slug) | .id')
  if [[ -z "$TENANT_ID" || "$TENANT_ID" == "null" ]]; then
    log_err "tenant lookup failed — body was:"
    printf '%s\n' "$TENANT_BODY" >&2
    exit 1
  fi
  log_info "reusing tenant id=$TENANT_ID"
else
  log_err "tenant creation failed (HTTP $TENANT_CODE):"
  printf '%s\n' "$TENANT_BODY" >&2
  exit 1
fi

# 5. Mint the per-tenant JWT every other stage uses ------------------------
JWT=$(mint_jwt "$JWT_SECRET" "$TENANT_ID" "demo-user" "tenant:admin" 86400)

save_env_var BASE_URL          "$BASE_URL"
save_env_var TENANT_ID         "$TENANT_ID"
save_env_var TENANT_SLUG       "$TENANT_SLUG"
save_env_var JWT               "$JWT"
save_env_var JWT_SECRET        "$JWT_SECRET"
save_env_var ORK_API_PID_FILE  "$PID_FILE"
save_env_var ORK_API_LOG       "$LOG_FILE"

log_info "wrote demo/.env (BASE_URL, TENANT_ID, JWT, ...)"

if [[ -f "$VITE_PID_FILE" ]]; then
  save_env_var WEBUI_VITE_PID_FILE "$VITE_PID_FILE"
  save_env_var WEBUI_VITE_LOG      "$WEBUI_VITE_LOG"
fi

banner "Stage 0 done"
log_info "ork-api: $BASE_URL  (logs: $LOG_FILE)"
log_info "tenant : $TENANT_SLUG ($TENANT_ID)"
if [[ -f "$VITE_PID_FILE" ]]; then
  log_info "web ui : $BASE_URL/  — paste the JWT from demo/.env in the ork Web UI (ADR-0017)"
fi
log_info "next   : make -C demo demo-stage-1   (or just 'make demo')"
