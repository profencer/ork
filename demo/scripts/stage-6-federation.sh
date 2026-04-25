#!/usr/bin/env bash
# Stage 6 — Federation: ork delegates to a vendor A2A peer (ADR 0007 + 0006).
#
# Spins up the stub peer (demo/peer-agent) on :8090, waits for its card to
# come up, then runs the `federation-demo` workflow. Both steps target the
# `vendor-planner` agent registered in demo/config/default.toml under
# `[[remote_agents]]`, so:
#
#   - the engine resolves `vendor-planner` through AgentRegistry,
#   - sends `message/send` JSON-RPC to the peer (ADR 0007),
#   - the final step's `delegate_to:` block forks an additional A2A call
#     against the peer (ADR 0006 §b).
#
# We tail both ork-api and peer logs at the end so the audience sees the
# request arriving at the peer and the reply flowing back.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 6 — Federation via vendor-planner peer (ADR 0007 + 0006)"

require_cmd cargo
require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

# 1. Make sure the peer is up (stage 0 starts it; this re-uses or revives) -
boot_peer_agent

log_info "peer card:"
curl -sS "$PEER_CARD_URL" \
  | jq '{name, description, version, url, capabilities, skills: [.skills[].id]}'
echo

# 2. Confirm ork-api picked the peer up via [[remote_agents]] ------------
log_info "verifying ork-api registered the remote agent (boot-time loader):"
sleep 1
curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/a2a/agents" \
  | jq 'map(select(.name | test("Vendor"; "i")) | {name, version, url})'
echo

# 3. Convert YAML and POST workflow definition ---------------------------
# `yaml_to_json` (lib.sh) tries mikefarah/yq, then python3+PyYAML, then a
# bundled `.json` sibling next to the YAML — federation-demo.json ships
# alongside federation-demo.yaml so the demo runs on a stock macOS box.
YAML="$DEMO_ROOT/workflows/federation-demo.yaml"
WF_JSON=$(yaml_to_json "$YAML")
log_info "POST $BASE_URL/api/workflows  (federation-demo)"
WF_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows" \
  -d "$WF_JSON")
WF_BODY=$(printf '%s' "$WF_RESP" | sed '$d')
WF_CODE=$(printf '%s' "$WF_RESP" | tail -n1)
if [[ "$WF_CODE" != "201" ]]; then
  log_err "federation workflow creation failed (HTTP $WF_CODE):"
  printf '%s\n' "$WF_BODY" >&2
  exit 1
fi
WF_ID=$(printf '%s' "$WF_BODY" | jq -r '.id')
log_info "created workflow id=$WF_ID"

# 4. Start a run ----------------------------------------------------------
TASK_TEXT="${FEDERATION_TASK:-Acme, please outline the rollout plan for the next region.}"
RUN_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows/$WF_ID/runs" \
  -d "$(jq -nc --arg t "$TASK_TEXT" '{input:{task:$t}}')")
RUN_BODY=$(printf '%s' "$RUN_RESP" | sed '$d')
RUN_CODE=$(printf '%s' "$RUN_RESP" | tail -n1)
if [[ "$RUN_CODE" != "201" ]]; then
  log_err "federation run start failed (HTTP $RUN_CODE):"
  printf '%s\n' "$RUN_BODY" >&2
  exit 1
fi
RUN_ID=$(printf '%s' "$RUN_BODY" | jq -r '.id')
log_info "started run id=$RUN_ID"

# 5. Poll until terminal --------------------------------------------------
TIMEOUT=${FEDERATION_TIMEOUT_SECS:-60}
start=$(date +%s)
last_status=""
while true; do
  RUN=$(curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/api/runs/$RUN_ID")
  status=$(printf '%s' "$RUN" | jq -r '.status')
  if [[ "$status" != "$last_status" ]]; then
    steps=$(printf '%s' "$RUN" | jq -r '.step_results | map(.step_id + ":" + (.status|tostring)) | join("  ")')
    log_info "run $RUN_ID -> $status  [$steps]"
    last_status=$status
  fi
  case "$status" in
    completed|failed|cancelled|rejected) break ;;
  esac
  if (( $(date +%s) - start > TIMEOUT )); then
    log_err "timed out after ${TIMEOUT}s waiting for terminal status (last=$status)."
    exit 1
  fi
  sleep 1
done

echo
log_info "step outputs:"
printf '%s' "$RUN" | jq -r '
  .step_results[] |
  "----- step \(.step_id|@text) (agent=\(.agent), status=\(.status)) -----\n\(.output // .error // "<empty>")"
'

echo
log_info "tail of peer-agent log (proves the round-trip arrived):"
tail -n 15 "$PEER_LOG" || true

banner "Stage 6 done"
log_info "next: make -C demo demo-stage-7"
