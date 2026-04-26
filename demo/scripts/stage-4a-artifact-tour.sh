#!/usr/bin/env bash
# Stage 4a — ADR-0016 artifact workflow (create → append → list/meta/load).
#
# Compiles `workflow-templates/artifact-tour.yaml`, POSTs it, runs three
# writer steps that exercise `create_artifact`, `append_artifact`, and
# `list_artifacts` / `artifact_meta` / `load_artifact`. Requires:
#   - `[artifacts] enabled` in `demo/config/default.toml` (stage 0 must have
#     been run after that block was added, or restart ork-api).
#   - `MINIMAX_API_KEY` (same contract as stage 4).
#
# Environment (optional):
#   ARTIFACT_DEMO_TASK   — input.task (default: short ADR-0016 blurb)
#   ARTIFACT_DEMO_NAME   — logical artifact name (default: demo/artifact-tour/notes)
#   ARTIFACT_DEMO_SEED   — first line of the created file
#   ARTIFACT_DEMO_APPEND — bytes appended in step 2 (default: newline + line1:…)
#   ARTIFACT_DEMO_EMBED_LABEL — `input.embed_variables.artifact_demo_label` for
#       ADR-0015 «var:artifact_demo_label» in the first step (see workflow YAML).
#   WORKFLOW_TIMEOUT_SECS — wall clock (default: 420 for three LLM steps)

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 4a — Artifact tour workflow (ADR-0016)"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

if [[ -z "${MINIMAX_API_KEY:-}" ]]; then
  log_warn "MINIMAX_API_KEY is not set — skipping the artifact tour."
  log_info "what stage 4a would have done:"
  log_info "  POST /api/workflows  (artifact-tour → JSON)"
  log_info "  POST /api/workflows/<id>/runs (input.artifact_name, seed_line, append_line, …)"
  log_info "  GET  /api/runs/<run_id> (poll)"
  exit 0
fi

YAML="$REPO_ROOT/workflow-templates/artifact-tour.yaml"
JSON_FALLBACK="$DEMO_ROOT/workflows/artifact-tour.json"
if [[ ! -f "$YAML" ]]; then
  log_err "missing $YAML — workflow template not found in workspace."
  exit 1
fi

WORKFLOW_JSON=$(yaml_to_json "$YAML" "$JSON_FALLBACK")
log_info "compiled $YAML to JSON ($(printf '%s' "$WORKFLOW_JSON" | wc -c | tr -d ' ') bytes)"

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

TASK_TEXT="${ARTIFACT_DEMO_TASK:-ADR-0016 artifact tour: create, append, then list/load}"
ARTIFACT_NAME="${ARTIFACT_DEMO_NAME:-demo/artifact-tour/notes}"
SEED_LINE="${ARTIFACT_DEMO_SEED:-line0: seeded in seed_file (ork demo)}"
DEFAULT_APPEND=$'\nline1: appended in append_file (ork demo)\n'
APPEND_LINE="${ARTIFACT_DEMO_APPEND:-$DEFAULT_APPEND}"
# Feeds `«var:artifact_demo_label»` in step `seed_file` (ADR-0015).
EMBED_LABEL="${ARTIFACT_DEMO_EMBED_LABEL:-"ork kitchen-sink (ADR-0015 + 0016)"}"

RUN_JSON=$(jq -nc \
  --arg t "$TASK_TEXT" \
  --arg n "$ARTIFACT_NAME" \
  --arg s "$SEED_LINE" \
  --arg a "$APPEND_LINE" \
  --arg ev "$EMBED_LABEL" \
  '{input:{task:$t, artifact_name:$n, seed_line:$s, append_line:$a, embed_variables:{artifact_demo_label:$ev}}}')

log_info "POST $BASE_URL/api/workflows/$WF_ID/runs"
log_info "input.task = $TASK_TEXT"
log_info "input.artifact_name = $ARTIFACT_NAME"
log_info "input.embed_variables.artifact_demo_label (ADR-0015 «var:…») = $EMBED_LABEL"
RUN_RESP=$(curl -sS -w '\n%{http_code}' \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -X POST "$BASE_URL/api/workflows/$WF_ID/runs" \
  -d "$RUN_JSON")
RUN_BODY=$(printf '%s' "$RUN_RESP" | sed '$d')
RUN_CODE=$(printf '%s' "$RUN_RESP" | tail -n1)
if [[ "$RUN_CODE" != "201" ]]; then
  log_err "workflow run start failed (HTTP $RUN_CODE):"
  printf '%s\n' "$RUN_BODY" >&2
  exit 1
fi
RUN_ID=$(printf '%s' "$RUN_BODY" | jq -r '.id')
log_info "started run id=$RUN_ID  (status=$(printf '%s' "$RUN_BODY" | jq -r '.status'))"

EXPECTED_STEPS=$(curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/api/workflows/$WF_ID" \
  | jq -r '.steps | length // 0')
log_info "workflow defines $EXPECTED_STEPS step(s); polling /api/runs/$RUN_ID until terminal"
API_LOG_FILE="${ORK_API_LOG:-$LOG_DIR/ork-api.log}"
if [[ -f "$API_LOG_FILE" ]]; then
  log_info "engine logs: tail -F $API_LOG_FILE  (filter for run id $RUN_ID for live progress)"
fi

# Poll (same contract as stage 4; default timeout a bit lower — three steps).
TIMEOUT=${WORKFLOW_TIMEOUT_SECS:-420}
DANGLING_GRACE_SECS=${DANGLING_GRACE_SECS:-15}
HEARTBEAT_SECS=${WORKFLOW_HEARTBEAT_SECS:-30}
start=$(date +%s)
all_terminal_since=0
last_status=""
last_steps=""
last_heartbeat=$start

fmt_steps_with_durations() {
  printf '%s' "$1" | jq -r '
    def secs(t):
      if t == null then null
      else (t | sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601)
      end;
    (now | floor) as $now_s
    | .step_results
    | map(
        .step_id as $id
        | .status as $st
        | (secs(.started_at)) as $s
        | (secs(.completed_at)) as $c
        | (if $s == null then null
           elif $c != null then $c - $s
           else $now_s - $s end) as $dur
        | ($id + ":" + $st + (
            if $dur != null and $dur > 0
            then "(" + ($dur|tostring) + "s)"
            else "" end
          ))
      )
    | join("  ")
  '
}

running_step_ids() {
  printf '%s' "$1" | jq -r '
    [.step_results[] | select(.status == "running") | .step_id] | join(", ")
  '
}

while true; do
  RUN=$(curl -sS -H "Authorization: Bearer $JWT" "$BASE_URL/api/runs/$RUN_ID")
  status=$(printf '%s' "$RUN" | jq -r '.status')
  steps=$(fmt_steps_with_durations "$RUN")
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
    last_heartbeat=$(date +%s)
  elif (( $(date +%s) - last_heartbeat >= HEARTBEAT_SECS )); then
    in_flight=$(running_step_ids "$RUN")
    log_info "still waiting (status=$status, ${EXPECTED_STEPS} expected, ${steps_terminal} terminal); in-flight: ${in_flight:-<none>}"
    last_heartbeat=$(date +%s)
  fi
  case "$status" in
    completed|failed|cancelled|rejected) break ;;
  esac
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
    log_err "timed out after ${TIMEOUT}s (artifact tour)."
    printf '%s\n' "$RUN" | jq '.step_results[]' >&2
    exit 1
  fi
  sleep 2
done

echo
log_info "final run status: $status"
echo
log_info "step summary:"
printf '%s' "$RUN" | jq -r '
  def secs(t):
    if t == null then null
    else (t | sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601)
    end;
  (now | floor) as $now_s
  | .step_results[]
  | (secs(.started_at)) as $s
  | (secs(.completed_at)) as $c
  | (if $s == null then "n/a"
     elif $c != null then (($c - $s)|tostring) + "s"
     else (($now_s - $s)|tostring) + "s" end) as $dur
  | "  \(.step_id|@text)  status=\(.status)  agent=\(.agent)  dur=\($dur)"
'

ERRORS_JSON=$(printf '%s' "$RUN" | jq -c '[.step_results[] | select(.error != null and .error != "") | {step_id, status, error}]')
if [[ "$ERRORS_JSON" != "[]" ]]; then
  echo
  log_warn "step errors:"
  printf '%s\n' "$ERRORS_JSON" | jq -r '.[] | "  \(.step_id) (\(.status)): \(.error)"'
fi

echo
log_info "output — seed_file (create_artifact):"
printf '%s' "$RUN" | jq -r '
  (.step_results[] | select(.step_id == "seed_file") | .output) // "(no seed_file output)"'
echo
log_info "output — append_file (load + append_artifact):"
printf '%s' "$RUN" | jq -r '
  (.step_results[] | select(.step_id == "append_file") | .output) // "(no append_file output)"'
echo
log_info "output — inventory (list + meta + load):"
printf '%s' "$RUN" | jq -r '
  (.step_results[] | select(.step_id == "inventory") | .output) // "(no inventory output)"'
echo
log_info "blobs for this run live under \`demo/data/artifacts\` (gitignored) when using the default FS backend."

banner "Stage 4a done"
log_info "next: make -C demo demo-stage-5"
