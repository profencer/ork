#!/usr/bin/env bash
# Stage 9 — LangGraph A2A peer: ork -> LangGraph (message/stream) and LangGraph -> ork (ask_ork / researcher).
#
# Requires: demo stage 0 (demo/.env, ork on :8080). Optional: MINIMAX_API_KEY for
# the LangGraph LLM. If the langgraph process is missing, exits 0 with a skip
# (same pattern as stage 1 without GITHUB_TOKEN).

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 9 — LangGraph bidirectional A2A peer (ADR 0007 + 0008)"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

LG_ADDR="${LG_ADDR:-127.0.0.1:8092}"
LG_CARD_URL="http://${LG_ADDR}/.well-known/agent-card.json"

boot_langgraph_agent

if [[ -n "${LG_SKIPPED:-}" && "${LG_SKIPPED}" == "1" ]]; then
  log_warn "langgraph-agent is not available (see stage 0 log) — skipping stage 9"
  exit 0
fi

if ! curl -s -o /dev/null --max-time 2 "$LG_CARD_URL"; then
  log_warn "no LangGraph card at $LG_CARD_URL — skipping stage 9"
  exit 0
fi

log_info "langgraph card:"
curl -sS "$LG_CARD_URL" | jq '{name, description, version, url, skills: [.skills[].id]}'
echo

log_info "verifying ork registered langgraph-researcher:"
curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/a2a/agents" \
  | jq 'map(select(.name | test("LangGraph"; "i")) | {name, version, url})' || true
echo

YAML="$DEMO_ROOT/workflows/langgraph-demo.yaml"
WF_JSON=$(yaml_to_json "$YAML")
log_info "POST $BASE_URL/api/workflows  (langgraph-demo)"
WF_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows" \
  -d "$WF_JSON")
WF_BODY=$(printf '%s' "$WF_RESP" | sed '$d')
WF_CODE=$(printf '%s' "$WF_RESP" | tail -n1)
if [[ "$WF_CODE" != "201" ]]; then
  log_err "langgraph workflow creation failed (HTTP $WF_CODE):"
  printf '%s\n' "$WF_BODY" >&2
  exit 1
fi
WF_ID=$(printf '%s' "$WF_BODY" | jq -r '.id')
log_info "created workflow id=$WF_ID"

TASK_TEXT="${LG_DEMO_TASK:-List two entry points in anthropic-sdk-typescript (e.g. main client or exports) the researcher can find with code_search.}" 
if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
  log_warn "MINIMAX_API_KEY not set — LangGraph LLM + researcher may fail; continuing"
fi

RUN_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows/$WF_ID/runs" \
  -d "$(jq -nc --arg t "$TASK_TEXT" \
    '{input:{task:$t, embed_variables:{demo_label:"ork demo (LangGraph ADR-0015)"}}}')")
RUN_BODY=$(printf '%s' "$RUN_RESP" | sed '$d')
RUN_CODE=$(printf '%s' "$RUN_RESP" | tail -n1)
if [[ "$RUN_CODE" != "201" ]]; then
  log_err "run start failed (HTTP $RUN_CODE):"
  printf '%s\n' "$RUN_BODY" >&2
  exit 1
fi
RUN_ID=$(printf '%s' "$RUN_BODY" | jq -r '.id')
log_info "started run id=$RUN_ID"

TIMEOUT=${LG_DEMO_TIMEOUT_SECS:-300}
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
    log_err "timed out after ${TIMEOUT}s (last=$status)"
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
log_info "tail of langgraph-agent log (inbound A2A + ask_ork):"
tail -n 25 "$LOG_DIR/langgraph-agent.log" 2>/dev/null || true

echo
if [[ "$status" == "completed" ]]; then
  log_info "grep ork reverse leg (researcher) in ork-api log:"
  if grep -E "a2a/agents/researcher|/researcher" "$LOG_DIR/ork-api.log" 2>/dev/null | tail -n 5; then
    true
  else
    log_warn "no explicit researcher line in ork-api.log — check JSON logs for run_id=$RUN_ID"
  fi
else
  log_warn "run did not complete successfully (status=$status)"
fi

banner "Stage 9 done"
log_info "next: make -C demo demo-stage-8  (or full teardown)"
