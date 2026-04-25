#!/usr/bin/env bash
# Stage 4 — Multi-step workflow run.
#
# Compiles a JSON form of `workflow-templates/change-plan.yaml` (5 steps), POSTs
# it to `POST /api/workflows`, kicks off a run with
# `POST /api/workflows/{id}/runs`, polls until terminal, and prints the markdown
# the writer step produced plus the reviewer verdict.
#
# The bundled JSON snapshot at `demo/workflows/change-plan.json` mirrors the YAML
# but drops the `delegate_to:` block on the `review` step. The engine currently
# FK-violates `a2a_tasks_parent_task_id_fkey` for `delegate_to:` because
# `WorkflowEngine::execute_agent_step` never inserts a parent `a2a_tasks` row;
# see demo/README.md "Known engine gaps" for the longer write-up.
#
# What this stage exercises end-to-end:
#   - `agent` step  (planner -> list_repos)
#   - `for_each`    (researcher loop over the planner's output)
#   - sequential `depends_on` between agent steps
#
# LLM dependency: this stage MUST hit a real LLM (planner.prompt produces JSON
# the rest of the steps depend on). If MINIMAX_API_KEY is unset, we exit 0 with
# a friendly skip message rather than dragging the whole `make demo` down.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 4 — Multi-step workflow run"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
  log_warn "MINIMAX_API_KEY is not set — skipping the workflow demo."
  log_warn "set MINIMAX_API_KEY (Minimax is the only wired LLM provider) to run this stage."
  log_info "what stage 4 would have done:"
  log_info "  POST /api/workflows           (change-plan.yaml -> JSON)"
  log_info "  POST /api/workflows/<id>/runs (input.task = ...)"
  log_info "  GET  /api/runs/<run_id>       (poll until terminal)"
  exit 0
fi

YAML="$REPO_ROOT/workflow-templates/change-plan.yaml"
JSON_FALLBACK="$DEMO_ROOT/workflows/change-plan.json"
if [[ ! -f "$YAML" ]]; then
  log_err "missing $YAML — workflow template not found in workspace."
  exit 1
fi

# 1. YAML -> JSON ---------------------------------------------------------
# `yaml_to_json` (lib.sh) prefers mikefarah/yq, falls back to python3+PyYAML,
# and finally to the bundled JSON snapshot we ship under demo/workflows/
# (so the demo doesn't need write access to workflow-templates/).
WORKFLOW_JSON=$(yaml_to_json "$YAML" "$JSON_FALLBACK")
log_info "compiled $YAML to JSON ($(printf '%s' "$WORKFLOW_JSON" | wc -c | tr -d ' ') bytes)"

# 2. POST /api/workflows --------------------------------------------------
log_info "POST $BASE_URL/api/workflows"
WF_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows" \
  -d "$WORKFLOW_JSON")
WF_BODY=$(printf '%s' "$WF_RESP" | sed '$d')
WF_CODE=$(printf '%s' "$WF_RESP" | tail -n1)
if [[ "$WF_CODE" != "201" ]]; then
  log_err "workflow creation failed (HTTP $WF_CODE):"
  printf '%s\n' "$WF_BODY" >&2
  exit 1
fi
WF_ID=$(printf '%s' "$WF_BODY" | jq -r '.id')
log_info "created workflow id=$WF_ID"

# 3. POST /api/workflows/{id}/runs ---------------------------------------
# The configured `[[repositories]]` entry in demo/config/default.toml points
# at the Anthropic TypeScript SDK, so the input task has to be answerable
# using that codebase. We ask for a self-contained, plausible feature
# (client-side request rate limiting on the SDK constructor) which has
# clear surface area: the client constructor options, the HTTP layer, and
# the streaming code path. Override with WORKFLOW_TASK=... to demo your own.
TASK_TEXT="${WORKFLOW_TASK:-Add an opt-in client-side request rate limiter to the Anthropic TypeScript SDK: extend the client constructor with a \\\`requestsPerMinute\\\` option, queue or reject requests that would exceed the budget, and document the new option in the README.}"
log_info "POST $BASE_URL/api/workflows/$WF_ID/runs"
log_info "input.task = \"$TASK_TEXT\""
RUN_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows/$WF_ID/runs" \
  -d "$(jq -nc --arg t "$TASK_TEXT" '{input:{task:$t}}')")
RUN_BODY=$(printf '%s' "$RUN_RESP" | sed '$d')
RUN_CODE=$(printf '%s' "$RUN_RESP" | tail -n1)
if [[ "$RUN_CODE" != "201" ]]; then
  log_err "workflow run start failed (HTTP $RUN_CODE):"
  printf '%s\n' "$RUN_BODY" >&2
  exit 1
fi
RUN_ID=$(printf '%s' "$RUN_BODY" | jq -r '.id')
log_info "started run id=$RUN_ID  (status=$(printf '%s' "$RUN_BODY" | jq -r '.status'))"

# Look up how many steps the workflow definition has so we can detect a
# real "engine left the run dangling" state. The /api/runs/{id} endpoint
# only returns the steps that have been *attempted* so far, so we'd
# otherwise mistake the brief gap between "step N completes" and
# "step N+1 enters running" for a finished run.
EXPECTED_STEPS=$(curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/api/workflows/$WF_ID" \
  | jq -r '.steps | length // 0')
log_info "workflow defines $EXPECTED_STEPS step(s); polling /api/runs/$RUN_ID until terminal"

# 4. Poll /api/runs/{id} --------------------------------------------------
# Exit conditions, in order of preference:
#   - the run reaches a terminal status (completed/failed/cancelled/rejected)
#   - every defined step has reached a terminal status AND the run has been
#     stuck at that all-terminal state for at least DANGLING_GRACE_SECS
#     (defends against engine edge cases where a per-step terminal-failed
#     leaves the run dangling at `running`)
#   - the script's own wall-clock budget runs out
TIMEOUT=${WORKFLOW_TIMEOUT_SECS:-180}
DANGLING_GRACE_SECS=${DANGLING_GRACE_SECS:-15}
start=$(date +%s)
all_terminal_since=0
last_status=""
last_steps=""
while true; do
  RUN=$(curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/api/runs/$RUN_ID")
  status=$(printf '%s' "$RUN" | jq -r '.status')
  steps=$(printf '%s' "$RUN" | jq -r '.step_results | map(.step_id + ":" + (.status|tostring)) | join("  ")')
  steps_count=$(printf '%s' "$RUN" | jq -r '.step_results | length')
  steps_terminal=$(printf '%s' "$RUN" | jq -r '
    .step_results
    | map(select(.status == "completed" or .status == "failed" or .status == "cancelled" or .status == "skipped"))
    | length
  ')
  if [[ "$status" != "$last_status" || "$steps" != "$last_steps" ]]; then
    log_info "run $RUN_ID -> $status  [$steps]"
    last_status=$status
    last_steps=$steps
  fi
  case "$status" in
    completed|failed|cancelled|rejected) break ;;
  esac

  # Dangling-run detector: only fire when *every defined step* is terminal
  # AND we've sat in that state for DANGLING_GRACE_SECS. Without the grace
  # period we'd race the engine between "step N completes" and "step N+1
  # appears in step_results".
  if (( EXPECTED_STEPS > 0 && steps_count == EXPECTED_STEPS && steps_terminal == EXPECTED_STEPS )); then
    if (( all_terminal_since == 0 )); then
      all_terminal_since=$(date +%s)
    elif (( $(date +%s) - all_terminal_since >= DANGLING_GRACE_SECS )); then
      log_warn "run status is still '$status' but all $EXPECTED_STEPS steps are terminal for ${DANGLING_GRACE_SECS}s+. Treating as terminal."
      break
    fi
  else
    all_terminal_since=0
  fi

  if (( $(date +%s) - start > TIMEOUT )); then
    log_err "timed out after ${TIMEOUT}s waiting for terminal status (last=$status)."
    printf '%s\n' "$RUN" | jq '.step_results[] | {step_id, status, error}' >&2
    exit 1
  fi
  sleep 2
done

# 5. Print the markdown the `write_plan` step produced. ------------------
echo
log_info "final run status: $status"
echo
log_info "step summary:"
printf '%s' "$RUN" | jq -r '.step_results[] | "  \(.step_id|@text)  status=\(.status)  agent=\(.agent)"'

# Surface per-step errors before the long markdown dump so they don't get lost.
ERRORS_JSON=$(printf '%s' "$RUN" | jq -c '[.step_results[] | select(.error != null and .error != "") | {step_id, status, error}]')
if [[ "$ERRORS_JSON" != "[]" ]]; then
  echo
  log_warn "step errors:"
  printf '%s\n' "$ERRORS_JSON" | jq -r '.[] | "  \(.step_id) (\(.status)): \(.error)"'
fi

echo
log_info "markdown produced by step \`write_plan\`:"
printf '%s' "$RUN" | jq -r '
  (.step_results[] | select(.step_id == "write_plan") | .output) // "(no write_plan output)"
'
echo
log_info "reviewer verdict:"
printf '%s' "$RUN" | jq -r '
  (.step_results[] | select(.step_id == "review") | .output) // "(no review output)"
'

banner "Stage 4 done"
log_info "next: make -C demo demo-stage-5"
