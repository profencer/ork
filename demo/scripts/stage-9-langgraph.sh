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
LG_LOG="${LOG_DIR}/langgraph-agent.log"
ORK_LOG="${LOG_DIR}/ork-api.log"

if [[ -f "$LG_LOG" ]]; then
  TRACE_LINE=$(grep '\[ask_ork\]' "$LG_LOG" | tail -n1 || true)
  if [[ -n "$TRACE_LINE" ]]; then
    RID=$(printf '%s' "$TRACE_LINE" | grep -oE 'run=[^ ]+' | head -1 | cut -d= -f2 || true)
    R_TASK_ID=$(printf '%s' "$TRACE_LINE" | grep -oE 'task_id=[^ ]+' | head -1 | cut -d= -f2 || true)
  else
    RID=""
    R_TASK_ID=""
  fi
else
  TRACE_LINE=""
  RID=""
  R_TASK_ID=""
fi

if [[ -n "$RID" ]]; then
  log_info "LangGraph ReAct trace (run=$RID, from langgraph-agent.log):"
  grep -E "run=${RID}" "$LG_LOG" 2>/dev/null | grep -E '\[trace agent\]|\[trace tools\]|\[ask_ork\]' || true
else
  if [[ "${LG_FOREIGN_PEER:-0}" == "1" ]]; then
    log_warn "no LangGraph trace: peer on ${LG_ADDR:-127.0.0.1:8092} is not demo-managed — stop it and run \`make -C demo demo-stage-0\` so logs go to $LG_LOG"
  elif [[ ! -s "$LG_LOG" ]]; then
    log_warn "no [ask_ork] line — $LG_LOG is missing or empty (demo-managed peer should append here)"
  else
    log_warn "no [ask_ork] line in $LG_LOG — trace unavailable (upgrade/restart langgraph-agent?)"
  fi
fi

echo
if [[ "$status" == "completed" && -n "${R_TASK_ID:-}" && "$R_TASK_ID" != "unknown" ]]; then
  log_info "Researcher A2A task (tasks/get, id=$R_TASK_ID):"
  if [[ -z "${TENANT_ID:-}" ]]; then
    log_warn "TENANT_ID unset — skipping tasks/get"
  else
    TG_RESP=$(curl -sS -w '\n%{http_code}' \
      -H "Authorization: Bearer $JWT" \
      -H "X-Tenant-Id: $TENANT_ID" \
      -H 'Content-Type: application/json' \
      -X POST "$BASE_URL/a2a/agents/researcher" \
      -d "$(jq -nc --arg tid "$R_TASK_ID" \
        '{jsonrpc:"2.0",id:1,method:"tasks/get",params:{id:$tid}}')")
    TG_BODY=$(printf '%s' "$TG_RESP" | sed '$d')
    TG_CODE=$(printf '%s' "$TG_RESP" | tail -n1)
    if [[ "$TG_CODE" != "200" ]]; then
      log_warn "tasks/get HTTP $TG_CODE"
      printf '%s\n' "$TG_BODY" >&2 || true
    else
      printf '%s' "$TG_BODY" | jq '
        if .error then {error: .error}
        else .result | {
          id,
          status,
          history: [.history[] | {
            role,
            parts: [.parts[] | if (.kind? // .type?) == "text" then
              {kind: "text", text: (.text | if length > 400 then .[0:400] + "..." else . end)}
            else . end]
          }]
        }
        end
      ' 2>/dev/null || printf '%s\n' "$TG_BODY"
    fi
  fi
elif [[ "$status" == "completed" ]]; then
  log_warn "researcher task_id missing or unknown — skipping tasks/get"
fi

echo
log_info "Researcher LLM output (from ork-api.log):"
if [[ -f "$ORK_LOG" ]]; then
  # Last block between researcher LLM banners (ORK_PRINT_LLM_OUTPUT=1 from stage 0).
  awk '
    BEGIN { lastcount = 0 }
    /========== LLM output \(researcher\) ==========/ {
      delete buf
      n = 1
      buf[1] = $0
      inblock = 1
      next
    }
    inblock && /========== end LLM output ==========/ {
      n++
      buf[n] = $0
      lastcount = n
      for (i = 1; i <= lastcount; i++) last[i] = buf[i]
      inblock = 0
      next
    }
    inblock {
      n++
      buf[n] = $0
    }
    END {
      for (i = 1; i <= lastcount; i++) print last[i]
    }
  ' "$ORK_LOG" | tail -n 80 || true
else
  log_warn "no ork-api.log — researcher LLM block unavailable"
fi

if [[ "$status" != "completed" ]]; then
  log_warn "run did not complete successfully (status=$status)"
fi

banner "Stage 9 done"
log_info "next: make -C demo demo-stage-8  (or full teardown)"
