#!/usr/bin/env bash
# Stage 8 — Teardown.
#
# Idempotent cleanup: kill the background processes the demo started
# (ork-api, peer-agent, langgraph-agent, webhook-receiver), drop the docker compose stack
# (postgres + redis) including its named volume, and remove the per-run
# state under demo/ (.env, logs/, data/, .last-hooks.json, ad-hoc PID
# files). Safe to run mid-demo or twice in a row.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

banner "Stage 8 — Teardown"

require_cmd docker

# 1. Kill background binaries --------------------------------------------
kill_pidfile "$DEMO_ROOT/.ork-api.pid"          "ork-api"
kill_pidfile "$DEMO_ROOT/.peer-agent.pid"        "peer-agent"
kill_pidfile "$DEMO_ROOT/.langgraph-agent.pid"    "langgraph-agent"
kill_pidfile "$DEMO_ROOT/.webhook-receiver.pid"  "webhook-receiver"
kill_pidfile "$DEMO_ROOT/.webui-vite.pid"        "webui vite (ADR-0017)"

# 2. Compose down (including the postgres named volume) -----------------
log_info "docker compose down -v"
docker compose -f "$DEMO_ROOT/docker-compose.yml" down -v --remove-orphans \
  || log_warn "compose down returned non-zero — ignore if the stack was already gone."

# 3. Remove generated state ---------------------------------------------
log_info "removing demo/.env, demo/logs, demo/data, demo/.last-hooks.json"
rm -f "$DEMO_ROOT/.env" "$DEMO_ROOT/.last-hooks.json"
rm -rf "$DEMO_ROOT/logs" "$DEMO_ROOT/data"

banner "Stage 8 done"
log_info "the demo is fully torn down. \`make demo\` will start fresh."
