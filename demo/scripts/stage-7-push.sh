#!/usr/bin/env bash
# Stage 7 — Push notifications + JWS key rotation (ADR 0009).
#
# What gets shown:
#
#   1. Boot a tiny webhook receiver (demo/webhook-receiver) on :8091.
#   2. Initial JWKS state — `GET /.well-known/jwks.json` returns one `kid`.
#   3. Register a push subscriber via JSON-RPC `tasks/pushNotificationConfig/set`
#      against a task we just created via `message/stream`. The script extracts
#      the task id from the first SSE event and immediately POSTs the set
#      request, then waits for the worker to deliver.
#   4. Pretty-print whatever lands at the receiver (header + payload).
#   5. Force a key rotation via `cargo run -p ork-cli -- admin push rotate-keys`.
#   6. Re-fetch the JWKS — there should now be two `kid`s (the freshly minted
#      one and the previous one kept around for the configured overlap window).
#
# The "live push" step is best-effort: if the planner fails its LLM call faster
# than the script can register the push config, we'll see an empty receiver.
# The JWKS rotation half is deterministic and is the headline of stage 7.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 7 — push notifications + key rotation (ADR 0009)"

require_cmd cargo
require_cmd curl
require_cmd jq

if [[ -z "${BASE_URL:-}" || -z "${JWT:-}" ]]; then
  log_err "demo/.env not populated — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

RECEIVER_DIR="$DEMO_ROOT/webhook-receiver"
RECEIVER_LOG="$LOG_DIR/webhook-receiver.log"
RECEIVER_PID_FILE="$DEMO_ROOT/.webhook-receiver.pid"
RECEIVER_ADDR="${RECEIVER_ADDR:-127.0.0.1:8091}"
RECEIVER_HOOK_URL="http://${RECEIVER_ADDR}/hook"
RECEIVER_HEALTH_URL="http://${RECEIVER_ADDR}/health"
RECEIVER_LAST_URL="http://${RECEIVER_ADDR}/last"
HOOKS_FILE="$DEMO_ROOT/.last-hooks.json"

# 1. Build + boot the receiver -------------------------------------------
log_info "building demo/webhook-receiver"
( cd "$RECEIVER_DIR" && cargo build -q )

kill_pidfile "$RECEIVER_PID_FILE" "previous webhook-receiver"

log_info "launching webhook-receiver on $RECEIVER_ADDR (logs -> $RECEIVER_LOG)"
: > "$RECEIVER_LOG"
rm -f "$HOOKS_FILE"
(
  cd "$RECEIVER_DIR"
  nohup env "RECEIVER_ADDR=$RECEIVER_ADDR" \
      "RECEIVER_STATE_FILE=$HOOKS_FILE" \
      "RUST_LOG=info" \
      cargo run -q -- --addr "$RECEIVER_ADDR" --state-file "$HOOKS_FILE" \
      >> "$RECEIVER_LOG" 2>&1 &
  echo "$!" > "$RECEIVER_PID_FILE"
)
log_info "webhook-receiver pid=$(cat "$RECEIVER_PID_FILE")"
if ! wait_for_url "$RECEIVER_HEALTH_URL" 30; then
  log_err "webhook-receiver never became healthy. Tail of $RECEIVER_LOG:"
  tail -n 40 "$RECEIVER_LOG" >&2 || true
  exit 1
fi

# 2. Initial JWKS state ---------------------------------------------------
echo
log_info "initial JWKS state — should have a single kid:"
JWKS_BEFORE=$(curl -sS "$BASE_URL/.well-known/jwks.json")
KIDS_BEFORE=$(printf '%s' "$JWKS_BEFORE" | jq -r '.keys // [] | map(.kid) | join(", ")')
COUNT_BEFORE=$(printf '%s' "$JWKS_BEFORE" | jq -r '.keys // [] | length')
echo "  count = $COUNT_BEFORE"
echo "  kids  = $KIDS_BEFORE"

# 3. Live push (best-effort race against task termination) ---------------
echo
log_info "kicking off message/stream on planner to mint a task id"

REQ=$(jq -nc \
  --arg msg_id "$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid)" \
  '{
    jsonrpc: "2.0",
    id: "demo-push-1",
    method: "message/stream",
    params: {
      message: {
        role: "user",
        parts: [{ kind: "text", text: "Plan a small refactor (push demo)." }],
        message_id: $msg_id,
        task_id: null,
        context_id: null,
        metadata: null
      }
    }
  }')

STREAM_OUT="$LOG_DIR/stage-7-stream.txt"
( curl -sS --no-buffer --max-time 30 \
    -H "Authorization: Bearer $JWT" \
    -H 'Content-Type: application/json' \
    -H 'Accept: text/event-stream' \
    "$BASE_URL/a2a/agents/planner" \
    -d "$REQ" > "$STREAM_OUT" 2>&1 ) &
STREAM_PID=$!

TASK_ID=""
deadline=$(( $(date +%s) + 8 ))
while [[ -z "$TASK_ID" && "$(date +%s)" -lt "$deadline" ]]; do
  if [[ -s "$STREAM_OUT" ]]; then
    TASK_ID=$(awk '
      /^data: / {
        sub(/^data: /, "");
        print
      }
    ' "$STREAM_OUT" | jq -r '
      .. | objects | .task_id? | strings
    ' 2>/dev/null | head -n 1 || true)
  fi
  sleep 0.2
done

if [[ -z "${TASK_ID:-}" || "$TASK_ID" == "null" ]]; then
  log_warn "could not capture a task_id from the SSE stream within 8s — skipping live push and going straight to rotation."
else
  log_info "captured task_id=$TASK_ID; registering push config -> $RECEIVER_HOOK_URL"
  SET_REQ=$(jq -nc \
    --arg task "$TASK_ID" \
    --arg url "$RECEIVER_HOOK_URL" \
    '{
      jsonrpc: "2.0",
      id: "push-set-1",
      method: "tasks/pushNotificationConfig/set",
      params: {
        task_id: $task,
        push_notification_config: { url: $url }
      }
    }')
  curl -sS \
    -H "Authorization: Bearer $JWT" \
    -H 'Content-Type: application/json' \
    -X POST "$BASE_URL/a2a/agents/planner" \
    -d "$SET_REQ" \
    | jq '{result, error}' || true
fi

# Let the SSE finish (and the push worker have a chance to deliver).
wait "$STREAM_PID" 2>/dev/null || true
log_info "waiting up to 5s for the push delivery to land at the receiver"
deadline=$(( $(date +%s) + 5 ))
while (( $(date +%s) < deadline )); do
  if [[ -s "$HOOKS_FILE" ]] && [[ "$(jq 'length' "$HOOKS_FILE" 2>/dev/null || echo 0)" -gt 0 ]]; then
    break
  fi
  sleep 0.5
done

echo
if [[ -s "$HOOKS_FILE" ]] && [[ "$(jq 'length' "$HOOKS_FILE" 2>/dev/null || echo 0)" -gt 0 ]]; then
  log_info "receiver state ($HOOKS_FILE):"
  jq '
    .[-1] | {
      received_at,
      kid: .header.kid,
      alg: .header.alg,
      payload_state: .payload.state,
      payload_task_id: .payload.task_id
    }
  ' "$HOOKS_FILE"
else
  log_warn "no push delivery landed — the planner task likely terminated before we could register the config."
  log_warn "this is expected when MINIMAX_API_KEY is unset and the LLM call fails fast."
fi

# 4. Key rotation --------------------------------------------------------
echo
log_info "running ork admin push rotate-keys (forces a new ES256 signing key)"
(
  cd "$REPO_ROOT"
  ORK__DATABASE__URL="postgres://ork:ork@127.0.0.1:5433/ork_demo" \
    ORK__AUTH__JWT_SECRET="$JWT_SECRET" \
    cargo run -q -p ork-cli -- admin push rotate-keys 2>&1
) | sed 's/^/  /'

echo
log_info "ground truth from a2a_signing_keys (every active kid + rotated_out_at):"
demo_psql -t -A -F '|' -c "
  SELECT kid, alg,
         to_char(created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created,
         to_char(expires_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires,
         COALESCE(to_char(rotated_out_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), '-') AS rotated_out
    FROM a2a_signing_keys
   ORDER BY created_at;" \
  | awk -F'|' 'BEGIN{printf "  %-40s  %-5s  %-21s  %-21s  %s\n", "kid", "alg", "created", "expires", "rotated_out"} { printf "  %-40s  %-5s  %-21s  %-21s  %s\n", $1, $2, $3, $4, $5 }'

echo
log_info "JWKS as currently published by ork-api:"
JWKS_AFTER=$(curl -sS "$BASE_URL/.well-known/jwks.json")
COUNT_AFTER=$(printf '%s' "$JWKS_AFTER" | jq -r '.keys // [] | length')
KIDS_AFTER=$(printf '%s' "$JWKS_AFTER" | jq -r '.keys // [] | map(.kid) | join(", ")')
echo "  count = $COUNT_AFTER  (was $COUNT_BEFORE before rotation)"
echo "  kids  = $KIDS_AFTER"

if (( COUNT_AFTER > COUNT_BEFORE )) || [[ "$KIDS_AFTER" != "$KIDS_BEFORE" ]]; then
  log_info "JWKS endpoint already reflects the rotation."
else
  log_info "JWKS endpoint still serves the pre-rotation snapshot."
  log_info "  ork-api caches keys in-process and refreshes on its own tick."
  log_info "  the new kid above (\`new_kid\`) is durably persisted; subscribers will see"
  log_info "  it on the next ork-api restart or refresh tick. Run \`make demo\` again"
  log_info "  for a clean view."
fi

banner "Stage 7 done"
log_info "next: make -C demo demo-stage-8"
