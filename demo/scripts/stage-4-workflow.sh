#!/usr/bin/env bash
# Stage 4 — Multi-step workflow run.
#
# Compiles a JSON form of `workflow-templates/change-plan.yaml` (5 steps), POSTs
# it to `POST /api/workflows`, kicks off a run with
# `POST /api/workflows/{id}/runs`, polls until terminal, and prints the markdown
# the writer step produced plus the reviewer verdict.
#
# The bundled JSON snapshot at `demo/workflows/change-plan.json` mirrors the YAML
# but drops the `delegate_to:` block on the `review` step. The underlying
# `a2a_tasks_parent_task_id_fkey` FK gap is now fixed
# (`crates/ork-core/tests/engine_persists_parent_task.rs`); the JSON snapshot is
# kept for now only because the `yq`/PyYAML compile path still needs a refresh
# pass. The canonical workflow lives in `workflow-templates/change-plan.yaml`.
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
  log_warn "the demo's default_provider is 'minimax' (configured in demo/config/default.toml)"
  log_warn "and reads its key from MINIMAX_API_KEY; swap in any OpenAI-compatible endpoint by"
  log_warn "editing [[llm.providers]] (see docs/adrs/0012-multi-llm-providers.md)."
  log_warn "the env var is sent verbatim as the Authorization header — set it to the literal"
  log_warn "header value, e.g. \`export MINIMAX_API_KEY=\"Bearer sk-…\"\` (a bare key 401s)."
  log_info "what stage 4 would have done:"
  log_info "  POST /api/workflows           (change-plan.yaml -> JSON)"
  log_info "  POST /api/workflows/<id>/runs (input.task = ...)"
  log_info "  GET  /api/runs/<run_id>       (poll until terminal)"
  exit 0
fi

if [[ "${MINIMAX_API_KEY}" != Bearer\ * ]]; then
  log_warn "MINIMAX_API_KEY is set but does not start with 'Bearer '."
  log_warn "ork sends the env var verbatim as the Authorization header (ADR 0012); a bare"
  log_warn "key produces a Minimax 401: 'Please carry the API secret key in the'"
  log_warn "'Authorization field of the request header (1004)'."
  log_warn "fix: re-export with the prefix, e.g. \`export MINIMAX_API_KEY=\"Bearer \$MINIMAX_API_KEY\"\`."
  log_warn "continuing anyway — the workflow will fail fast if Minimax rejects the request."
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
# Surface where the engine is actually logging so a stuck step can be
# diagnosed without trawling for the file path. The stage-0 bootstrap
# saves this into demo/.env.
API_LOG_FILE="${ORK_API_LOG:-$LOG_DIR/ork-api.log}"
if [[ -f "$API_LOG_FILE" ]]; then
  log_info "engine logs: tail -F $API_LOG_FILE  (filter for run id $RUN_ID for live progress)"
fi

# 4. Poll /api/runs/{id} --------------------------------------------------
# Exit conditions, in order of preference:
#   - the run reaches a terminal status (completed/failed/cancelled/rejected)
#   - every defined step has reached a terminal status AND the run has been
#     stuck at that all-terminal state for at least DANGLING_GRACE_SECS
#     (defends against engine edge cases where a per-step terminal-failed
#     leaves the run dangling at `running`)
#   - the script's own wall-clock budget runs out
#
# Default is 600s because change-plan.yaml has 5 LLM-driven steps, the
# slowest of which (`research_repos` with up to 5 tool-loop iterations
# against the live Minimax upstream) routinely lands in the 60-90s
# range on its own. A single observed run on 2026-04-25 took 187s
# end-to-end with healthy upstream, so the previous 180s budget was
# clipping legitimate completions by single-digit seconds. Operators
# pinning a faster catalog can override via WORKFLOW_TIMEOUT_SECS.
TIMEOUT=${WORKFLOW_TIMEOUT_SECS:-600}
DANGLING_GRACE_SECS=${DANGLING_GRACE_SECS:-15}
HEARTBEAT_SECS=${WORKFLOW_HEARTBEAT_SECS:-30}
start=$(date +%s)
all_terminal_since=0
last_status=""
last_steps=""
last_heartbeat=$start

# Render `<step_id>:<status>(<dur>)` for every entry in `.step_results`.
# Running steps get the live elapsed since `started_at`; terminal steps
# get `started_at -> completed_at` if both are present. This is what the
# user actually wants to see while waiting — "is the slow step the LLM
# call or am I waiting on a finished step that just hasn't bubbled up?"
fmt_steps_with_durations() {
  printf '%s' "$1" | jq -r '
    # Engine emits sub-second precision (`2026-04-25T12:34:48.547477Z`)
    # which `fromdateiso8601` rejects (it only handles `%Y-%m-%dT%H:%M:%SZ`).
    # Strip the fractional component before parsing. `null` (pending step)
    # propagates so the consumer can decide what to render.
    def secs(t):
      if t == null then null
      else (t | sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601)
      end;
    # NOTE: must NOT name this "now" — that shadows jq builtin `now`
    # and produces infinite recursion (jq aborts with OOM).
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

# Step ids currently in `running`, joined for the heartbeat line.
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
    # No state change for HEARTBEAT_SECS — call out which step we're
    # actually waiting on so the user knows it isn't the polling loop
    # that's stuck.
    in_flight=$(running_step_ids "$RUN")
    log_info "still waiting (status=$status, ${EXPECTED_STEPS} expected, ${steps_terminal} terminal); in-flight: ${in_flight:-<none>}"
    last_heartbeat=$(date +%s)
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
    log_err "step_results (full):"
    printf '%s\n' "$RUN" | jq '.step_results[] | {
      step_id, agent, status, error,
      started_at, completed_at,
      output_chars: (.output // "" | length)
    }' >&2
    in_flight=$(running_step_ids "$RUN")
    if [[ -n "$in_flight" ]]; then
      log_err "still running: $in_flight"
    fi
    if [[ -f "$API_LOG_FILE" ]]; then
      # Surface the last engine-side breadcrumbs for this run so the
      # user doesn't have to know about $API_LOG_FILE. Filter to lines
      # that mention either the run id or any in-flight step id; fall
      # back to the unfiltered tail if the filter found nothing
      # (engine traces don't always carry the run id).
      log_err "tail of $API_LOG_FILE (filtered to run $RUN_ID / in-flight steps):"
      filter_re="$RUN_ID"
      if [[ -n "$in_flight" ]]; then
        filter_re="$filter_re|$(printf '%s' "$in_flight" | sed 's/, /|/g')"
      fi
      filtered=$(grep -E "$filter_re" "$API_LOG_FILE" 2>/dev/null | tail -n 80 || true)
      if [[ -n "$filtered" ]]; then
        printf '%s\n' "$filtered" >&2
      else
        log_err "(no run-id-tagged lines; last 80 lines unfiltered)"
        tail -n 80 "$API_LOG_FILE" >&2 || true
      fi
    else
      log_err "(no $API_LOG_FILE — engine isn't logging to a file)"
    fi
    exit 1
  fi
  sleep 2
done

# 5. Print the markdown the `write_plan` step produced. ------------------
echo
log_info "final run status: $status"
echo
log_info "step summary:"
printf '%s' "$RUN" | jq -r '
  # See fmt_steps_with_durations: strip sub-second precision, propagate
  # null for pending steps, and avoid shadowing jq builtin `now`.
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
