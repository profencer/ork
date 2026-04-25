#!/usr/bin/env bash
# Stage 3 — A2A `message/stream` + SSE replay (ADR 0008).
#
# Workflow:
#   1. POST a JSON-RPC `message/stream` request to /a2a/agents/planner with
#      `curl --max-time 1` so curl drops the connection mid-stream.
#   2. Parse the task id out of the captured SSE chunks.
#   3. Reconnect to GET /a2a/agents/planner/stream/{task_id} — the server's
#      replay buffer should serve the same events back, proving the SSE
#      bridge can resume an interrupted stream.
#
# Note: stage 3 does not need MINIMAX_API_KEY. Even if the planner agent's
# LLM call fails (no key), the dispatcher emits at least a `Working` status
# update, persists the task, and writes those events into the replay buffer
# — which is exactly what we want to demonstrate here.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 3 — message/stream + SSE replay (ADR 0008)"

require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

STREAM_OUT="$LOG_DIR/stage-3-initial-stream.txt"
REPLAY_OUT="$LOG_DIR/stage-3-replay.txt"

REQ=$(jq -nc \
  --arg msg_id "$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid)" \
  '{
    jsonrpc: "2.0",
    id: "demo-stream-1",
    method: "message/stream",
    params: {
      message: {
        role: "user",
        parts: [{ kind: "text", text: "Sketch how you would add an opt-in client-side request rate limiter to the Anthropic TypeScript SDK." }],
        message_id: $msg_id,
        task_id: null,
        context_id: null,
        metadata: null
      },
      configuration: { blocking: false, history_length: 10 },
      metadata: null
    }
  }')

# 1. Initial stream — kill curl after a short window. ----------------------
log_info "POST $BASE_URL/a2a/agents/planner  (message/stream, --max-time 2)"
log_info "we'll cut the connection after 2s and reconnect via the replay endpoint."
echo

set +e
curl -sS --no-buffer --max-time 2 \
  -H "Authorization: Bearer $JWT" \
  -H 'Content-Type: application/json' \
  -H 'Accept: text/event-stream' \
  "$BASE_URL/a2a/agents/planner" \
  -d "$REQ" > "$STREAM_OUT"
CURL_EXIT=$?
set -e

if [[ ! -s "$STREAM_OUT" ]]; then
  log_err "no SSE chunks captured (curl exit=$CURL_EXIT). Tail of ork-api log:"
  tail -n 40 "${ORK_API_LOG:-$LOG_DIR/ork-api.log}" >&2 || true
  exit 1
fi

log_info "captured $(grep -c '^data: ' "$STREAM_OUT" || true) data chunks before disconnect:"
sed -n '1,30p' "$STREAM_OUT"
echo

# 2. Extract the task id. The `Working` status update emitted first carries
# `task_id` (snake_case in this build). Walk every nested object so we catch
# it whether it lives on a StatusUpdate or on the Message echo.
TASK_ID=$(awk '
  /^data: / {
    sub(/^data: /, "");
    print
  }
' "$STREAM_OUT" | jq -r '
  .. | objects | .task_id? | strings
' 2>/dev/null | head -n 1)

if [[ -z "${TASK_ID:-}" || "$TASK_ID" == "null" ]]; then
  log_warn "could not parse taskId from stream — falling back to /a2a/agents/planner registry."
  log_warn "stage 3 cannot continue without a task id; tail of stream:"
  cat "$STREAM_OUT" >&2
  exit 1
fi
log_info "captured task id: $TASK_ID"

# 3. Reconnect via the replay endpoint. -----------------------------------
echo
log_info "GET $BASE_URL/a2a/agents/planner/stream/$TASK_ID"
log_info "this is the replay endpoint — same task, same buffered events."
echo

set +e
curl -sS --no-buffer --max-time 3 \
  -H "Authorization: Bearer $JWT" \
  -H 'Accept: text/event-stream' \
  "$BASE_URL/a2a/agents/planner/stream/$TASK_ID" > "$REPLAY_OUT"
set -e

if [[ ! -s "$REPLAY_OUT" ]]; then
  log_err "replay endpoint returned no data."
  exit 1
fi
log_info "replay returned $(grep -c '^data: ' "$REPLAY_OUT" || true) data chunks:"
sed -n '1,30p' "$REPLAY_OUT"

banner "Stage 3 done"
log_info "see full output in $STREAM_OUT and $REPLAY_OUT"
log_info "next: make -C demo demo-stage-4"
