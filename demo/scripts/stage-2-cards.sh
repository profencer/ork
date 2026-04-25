#!/usr/bin/env bash
# Stage 2 — A2A agent cards + registry.
#
# Demonstrates ADR 0003 (A2A wire types) + ADR 0005 (card publishing &
# discovery): every locally-registered agent publishes its `AgentCard` JSON
# at a well-known URL, plus the convenience listing at `GET /a2a/agents`.
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

banner "Stage 2 — A2A cards and registry (ADR 0003 + 0005)"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
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

banner "Stage 2 done"
log_info "next: make -C demo demo-stage-3"
