#!/usr/bin/env bash
# Stage 1 — `ork standup` against a real GitHub repo.
#
# Goal: prove the CLI surface (`ork-cli`) and the GitHub integration adapter
# (`ork-integrations::github::GitHubAdapter`) work without an LLM. We use
# `--raw` so no `MINIMAX_API_KEY` is required for stage 1; the LLM-driven
# stages start at stage 4.
#
# Repo choice: `tokio-rs/tokio` is a busy public repo with a constant stream
# of commits / PRs, so the 168-hour window almost always returns interesting
# output. Override via `STANDUP_REPO=<owner>/<repo>` on the command line.
#
# The script exits 0 with a friendly skip message when no GitHub token is
# present so `make demo` can keep going. The plan flagged this in
# stage-1's "what we're showing" caveat.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
load_env

banner "Stage 1 — ork CLI: standup brief"

require_cmd cargo

REPO="${STANDUP_REPO:-tokio-rs/tokio}"
HOURS="${STANDUP_HOURS:-168}"

if [[ -z "${GITHUB_TOKEN:-}" && -z "${GITLAB_TOKEN:-}" ]]; then
  log_warn "neither GITHUB_TOKEN nor GITLAB_TOKEN is set — skipping the standup demo."
  log_warn "set GITHUB_TOKEN to run this stage; everything else still works."
  log_info "what stage 1 would have done:"
  log_info "  cargo run -q -p ork-cli -- standup $REPO --hours $HOURS --raw"
  exit 0
fi

log_info "running: cargo run -q -p ork-cli -- standup $REPO --hours $HOURS --raw"
log_info "(this hits the live GitHub API; output is verbatim from ork-cli)"
echo

# Run from REPO_ROOT so cargo finds the workspace and the CLI picks up
# `config/default.toml` (we don't need the demo overrides for this stage —
# standup talks to GitHub, not to ork-api).
cd "$REPO_ROOT"
cargo run -q -p ork-cli -- standup "$REPO" --hours "$HOURS" --raw

banner "Stage 1 done"
log_info "next: make -C demo demo-stage-2"
