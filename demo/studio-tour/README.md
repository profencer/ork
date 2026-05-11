# Studio tour — post-pivot ork demo

A self-contained, single-binary showcase for the code-first ork platform
([ADR-0048](../../docs/adrs/0048-pivot-to-code-first-rig-platform.md)).
One `cargo run` boots:

- [`OrkApp`](../../docs/adrs/0049-orkapp-central-registry.md) with 2 agents
  (`concierge`, `analyst`), 2 native tools (`clock-now`, `dice-roll`), 1
  branching workflow (`feedback-triage` — classify a customer message
  via the analyst, then branch into apology / thanks / acknowledge), and
  libsql memory.
- The [ADR-0056](../../docs/adrs/0056-auto-generated-rest-and-sse-surface.md)
  auto-generated REST/SSE surface at `/api/...`.
- [Studio](../../docs/adrs/0055-studio-local-dev-ui.md) at
  `http://127.0.0.1:4111/studio` with all six v1 panels live.
- A demo-only `POST /demo/seed` route (the "Demo data" button) that
  pre-seeds 2 memory threads + 12 synthetic scorer rows so every Studio
  panel has something to show on first open.

Unlike the kitchen-sink demo one level up, this one is **one process,
one command, no Docker**. The pre-pivot demo stays put for the
A2A wire / langgraph / federation story.

## Prerequisites

| Tool | Why | How to check |
| --- | --- | --- |
| Rust toolchain (workspace pinned) | builds the demo binary | `cargo --version` |
| `curl` + `jq` | poke the demo's REST surface | `curl --version` |
| An OpenAI-compatible API key | the agents call a real LLM | see `.env.example` |

The demo refuses to boot without a key, on purpose — running Studio
against a broken LLM hides the most interesting trace data.

## TL;DR

```bash
# from this directory
cp .env.example .env             # then edit and uncomment one key block
set -a; source .env; set +a      # export the chosen vars
cargo run                        # builds + boots; first build ≈ 1m

# in a second terminal:
curl -sf -X POST http://127.0.0.1:4111/demo/seed | jq
open http://127.0.0.1:4111/studio  # macOS; substitute xdg-open on Linux
```

That's the whole loop. Stop with `Ctrl-C`.

## What each Studio panel will show

After the seed call:

| Panel | What you'll see |
| --- | --- |
| **Overview** | `concierge` and `analyst` listed under agents; `clock-now` and `dice-roll` under tools; `feedback-triage` under workflows; libsql under memory; two `exact_match` scorer bindings under scorers. |
| **Chat** | Type "what time is it?" or "roll 3d20" — the `concierge` agent will call the corresponding tool. The SSE stream renders deltas + a `tool_call` chip per [ADR-0056 §`Streaming`](../../docs/adrs/0056-auto-generated-rest-and-sse-surface.md). |
| **Workflows** | Click "Run" on `feedback-triage` with `{"message":"my order arrived smashed!"}` (negative) or `{"message":"your concierge agent is delightful"}` (positive) — the workflow classifies via the analyst, branches, drafts a reply via the concierge, and logs the final structured output to the binary's stderr. Two LLM calls per run. The "Past runs" list is in-session only (server-side history is the ADR-0056 M1 follow-up). |
| **Memory** | Two seeded threads under `demo-resource` — click "Delete" to verify [ADR-0053](../../docs/adrs/0053-memory-working-and-semantic.md)'s `mem_messages` + `mem_embeddings` cleanup live. |
| **Scorers** | Aggregate table over the 12 seeded rows: pass-rate ≈ 66% for `exact_match`, ≈ 50% for `latency_under`. Drilling into rows lights up the ADR-0054 sink. |
| **Evals** | Point `dataset` at `./data/concierge-evals.jsonl`, `agent` at `concierge`, `scorer spec` at `exact_match=answer`, click Run — see `examples=3, passed=2, failed=1`. |

The Traces and Logs panels return `501 Not Implemented` with a
deferral envelope — see ADR-0055's `## Reviewer findings` for the
follow-up observability ADR plan.

## Knobs

- `ORK_DEMO_LLM_BASE_URL` / `ORK_DEMO_LLM_MODEL` — point the demo at any
  OpenAI-compatible endpoint (Anthropic via OpenRouter, Minimax,
  vLLM, etc.).
- `ORK_DEMO_DB_PATH` — change where the libsql memory file lands.
- `RUST_LOG` — `info,ork_studio=debug` is a useful next-step filter
  while exploring the diff.

## Inner-loop story

This demo also exercises the [ADR-0057](../../docs/adrs/0057-ork-cli-dev-build-start.md)
dev verbs. Once `ork` is on `PATH` (`cargo install --path
../../crates/ork-cli`), the equivalent of the bare `cargo run` above
is `ork dev` from this directory — which adds file watching, browser
auto-open, and the bundle-hash-cached Studio rebuild.

## Files

- [`Cargo.toml`](Cargo.toml) — excluded-from-workspace path deps on every ork crate the demo touches.
- [`src/main.rs`](src/main.rs) — builds the `OrkApp`, merges the
  auto / Studio / `/demo` routers, binds 127.0.0.1:4111.
- [`src/agents.rs`](src/agents.rs) — `concierge` + `analyst`
  `CodeAgent`s; LLM provider resolution from env.
- [`src/tools.rs`](src/tools.rs) — `clock-now` + `dice-roll` native tools.
- [`src/workflows.rs`](src/workflows.rs) — `feedback-triage` workflow: classify → branch (apology / thanks / acknowledge) → finalize.
- [`src/seed.rs`](src/seed.rs) — `POST /demo/seed` route.
- [`data/concierge-evals.jsonl`](data/concierge-evals.jsonl) — 3-example
  fixture for the Evals panel.

## When NOT to use this demo

If you need to demonstrate the **A2A wire** (peer agents, JSON-RPC,
push notifications, federation, langgraph interop), use the kitchen-
sink demo at [`../`](../README.md) instead — that's still the
canonical story for ADR-0001..0010 + ADR-0013 + ADR-0016.
