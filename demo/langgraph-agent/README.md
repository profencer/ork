# demo-langgraph-agent

Python LangGraph peer for the ork **kitchen-sink** demo. Speaks the same A2A 1.0 
JSON shape as `ork-a2a` (snake_case) — not `a2a-sdk` — so ork’s `A2aRemoteAgent` and 
`CardFetcher` work without a translation layer.

## Run locally

```bash
cd demo/langgraph-agent
python3.12 -m venv .venv && . .venv/bin/activate
pip install -e ".[dev]"
export OPENAI_BASE_URL="https://api.minimax.io/v1"
# OPENAI_API_KEY: bare key (strip `Bearer` if using MINIMAX-style header value)
export OPENAI_API_KEY="..."
export ORK_JWT=...            # or rely on DEMO_ROOT + ../.env
export ORK_TENANT_ID=...
export ORK_BASE_URL="http://127.0.0.1:8080"
export DEMO_ROOT="$(pwd)/../"
python -m demo_langgraph_agent --addr 127.0.0.1:8092
```

## Test

```bash
ruff check src tests
pytest -q
```

## Wire

- Inbound: `GET /.well-known/agent-card.json`, `POST /` (JSON-RPC `message/send` + `message/stream`, SSE = bare `TaskEvent` JSON per `data:` line, same as `demo/peer-agent`).
- Outbound: `ask_ork` tool → `POST {ORK_BASE_URL}/a2a/agents/researcher` with Authorization + `X-Tenant-Id`.
