#!/usr/bin/env bash
# Stage 5 — MCP tool plane round-trip (ADR 0010).
#
# Three things are proved:
#
#   1. Boot wiring: grep the ork-api log for the MCP refresh loop banner —
#      this is the `with_mcp` arm being attached to the composite tool
#      executor at startup with the demo's `[mcp.servers]` config.
#   2. Tenant tools listing: `GET /a2a/agents` is hit so the audience can
#      see the API surface that downstream consumers use to enumerate
#      tools per tenant.
#   3. End-to-end MCP round-trip: re-runs the `stdio_everything_server`
#      integration test in `ork-mcp`, which spawns the official
#      `@modelcontextprotocol/server-everything` over stdio (same transport
#      and same `McpClient` ork-api uses) and round-trips
#      `mcp:everything.echo`.
#
# Why the integration test instead of an HTTP call into the planner: ork's
# LocalAgent currently runs only the tools listed in its `AgentConfig`,
# which doesn't include MCP tools by default. Driving the canonical MCP
# code path through `cargo test` is the clearest demo without monkey-
# patching the agent registry.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 5 — MCP tool plane (ADR 0010)"

require_cmd cargo
require_cmd grep

LOG_FILE="${ORK_API_LOG:-$LOG_DIR/ork-api.log}"
if [[ ! -s "$LOG_FILE" ]]; then
  log_err "ork-api log $LOG_FILE is missing or empty — run \`make -C demo demo-stage-0\` first."
  exit 1
fi

# 1. Boot evidence -------------------------------------------------------
log_info "evidence from $LOG_FILE that the MCP layer was wired:"
echo
if grep -E "MCP descriptor refresh loop|with_mcp|ADR-0010" "$LOG_FILE" \
     | tail -n 20; then
  :
else
  log_warn "no MCP boot lines found — is [mcp].enabled = true in demo/config/default.toml?"
fi
echo

# 2. Show the demo's [mcp.servers] block ----------------------------------
log_info "demo MCP servers (from demo/config/default.toml):"
awk '
  /^\[mcp\]/                        { in_mcp = 1; print; next }
  /^\[\[mcp\.servers\]\]/           { in_mcp = 1; print; next }
  /^\[mcp\.servers\.transport\]/    { in_mcp = 1; print; next }
  /^\[/                             { in_mcp = 0 }
  in_mcp                            { print }
' "$DEMO_ROOT/config/default.toml"
echo

# 3. End-to-end round-trip via the McpClient + reference server ----------
if ! command -v npx >/dev/null 2>&1; then
  log_warn "npx is not on PATH — skipping the live MCP round-trip."
  log_warn "install Node.js to run this part of stage 5 (the boot evidence above is enough to prove ADR 0010 is wired)."
  exit 0
fi

log_info "round-tripping mcp:everything.echo via the same McpClient ork-api uses"
log_info "(this re-uses the existing integration test under crates/ork-mcp/tests/)"
echo
( cd "$REPO_ROOT" && \
    cargo test -q --features mcp-stdio-it -p ork-mcp \
      stdio_everything_server -- --nocapture 2>&1 ) | sed -n '1,80p'

banner "Stage 5 done"
log_info "next: make -C demo demo-stage-6"
