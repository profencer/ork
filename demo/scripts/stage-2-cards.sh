#!/usr/bin/env bash
# Stage 2 — A2A agent cards + registry + ADR-0013 gateway smoke tests.
#
# Demonstrates ADR 0003 (A2A wire types) + ADR 0005 (card publishing &
# discovery): every locally-registered agent publishes its `AgentCard` JSON
# at a well-known URL, plus the convenience listing at `GET /a2a/agents`.
# Also hits the generic `rest` and `webhook` gateway routes mounted under
# `/api/gateways/...` (public routes; `X-Tenant-Id` carries the demo tenant).
#
# What we expect to see:
#   - the bare `/.well-known/agent-card.json` returns the planner card (set
#     via `discovery.default_agent_id = "planner"` in demo/config/default.toml).
#   - per-agent `/.well-known/agent-card.json` works for every built-in role.
#   - the protected `GET /a2a/agents` endpoint returns 5 cards: planner,
#     researcher, writer, reviewer, synthesizer (seeded by
#     `crates/ork-agents/src/roles.rs`).
#   - if stage 6 has already booted the peer, `vendor-planner` will appear
#     too as a 6th remote card.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 2 — A2A cards, registry, and gateways (ADR 0003 + 0005 + 0013)"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" || -z "${TENANT_ID:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

# 1. Public well-known default card -------------------------------------
log_info "GET $BASE_URL/.well-known/agent-card.json (public, no auth)"
curl -sS "$BASE_URL/.well-known/agent-card.json" \
  | jq '{name, description, version, url, capabilities, skills: [.skills[].id]}'

echo

# 2. Per-agent well-known cards -----------------------------------------
log_info "GET /.well-known/agent-card.json for each built-in role:"
for agent in planner researcher writer reviewer synthesizer; do
  printf '  %-12s -> ' "$agent"
  curl -sS "$BASE_URL/a2a/agents/$agent/.well-known/agent-card.json" \
    | jq -r '"\(.name)  (\(.skills | length) skills, streaming=\(.capabilities.streaming))"' \
    || echo '(not available)'
done

echo

# 3. Protected registry listing -----------------------------------------
log_info "GET $BASE_URL/a2a/agents (protected — requires Bearer)"
curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/a2a/agents" \
  | jq 'map({name, version, streaming: .capabilities.streaming, push: .capabilities.push_notifications, skills: (.skills | length)})'

echo

# 4. ADR-0013 — generic gateways (public routes; per-request tenant) ------------
log_info "ADR-0013: REST gateway (demo-rest) without X-Tenant-Id → 401"
REST_CODE_UNAUTH=$(curl -sS -o /dev/null -w '%{http_code}' \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/gateways/rest/demo-rest" \
  -d '{"message":"ping from demo"}')
if [[ "$REST_CODE_UNAUTH" != "401" ]]; then
  log_err "expected HTTP 401 without tenant, got $REST_CODE_UNAUTH"
  exit 1
fi
log_info "  -> HTTP $REST_CODE_UNAUTH (expected 401)"

if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
  log_info "ADR-0013: REST gateway with X-Tenant-Id (expects 200; LLM will run)"
  curl -sS -o "$LOG_DIR/gateway-rest.json" -w "  -> HTTP %{http_code}\n" \
    -H "X-Tenant-Id: $TENANT_ID" \
    -H 'Content-Type: application/json' \
    -X POST "$BASE_URL/api/gateways/rest/demo-rest" \
    -d '{"message":"Reply with a single word: ok"}' \
    || true
  head -c 200 "$LOG_DIR/gateway-rest.json" 2>/dev/null | tr -d '\n' || true
  printf '\n\n'
else
  log_info "ADR-0013: skipping REST 200/LLM round-trip (set MINIMAX_API_KEY to exercise)"
fi

log_info "ADR-0013: Webhook gateway (demo-webhook) fire-and-forget → 202"
WH_CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
  -H "X-Tenant-Id: $TENANT_ID" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/gateways/webhook/demo-webhook" \
  -d '{"event":"adeo_demo","payload":{"source":"stage-2"}}')
if [[ "$WH_CODE" != "202" ]]; then
  log_err "expected HTTP 202 from webhook gateway, got $WH_CODE"
  exit 1
fi
log_info "  -> HTTP $WH_CODE (body accepted; agent work runs in background)"

banner "Stage 2 done"
log_info "next: make -C demo demo-stage-3"
