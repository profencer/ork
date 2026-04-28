"""Call back into ork's A2A JSON-RPC as a client (reverse leg of the demo)."""

from __future__ import annotations

import json
import logging
import re
import uuid
from pathlib import Path
from typing import Any

import httpx

from demo_langgraph_agent.a2a_types import Message, PartText, Role, Task

log = logging.getLogger(__name__)


def _load_dotenv_file(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.is_file():
        return out
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", line)
        if m:
            k, v = m.group(1), m.group(2).strip()
            if (v.startswith('"') and v.endswith('"')) or (v.startswith("'") and v.endswith("'")):
                v = v[1:-1]
            out[k] = v
    return out


def _env(key: str) -> str:
    import os

    v = os.environ.get(key)
    return (v or "").strip()


# Connect/write/pool stay short so a wedged TCP path fails fast; read is long because
# ork's `researcher` runs its own LLM round-trip + MCP tool loop and can take >120s
# on a cold demo (Postgres warming, MiniMax cold start, code_search over a real repo).
# The outer demo poll budget is `LG_DEMO_TIMEOUT_SECS` (default 300s), so default to
# the same here and let it be overridden via env.
_DEFAULT_ASK_READ_TIMEOUT_SECS = 300.0
_DEFAULT_ASK_CONNECT_TIMEOUT_SECS = 10.0
_DEFAULT_ASK_WRITE_TIMEOUT_SECS = 30.0
_DEFAULT_ASK_POOL_TIMEOUT_SECS = 10.0


def _resolve_ask_timeout() -> httpx.Timeout:
    raw = _env("ORK_ASK_TIMEOUT_SECS")
    read = _DEFAULT_ASK_READ_TIMEOUT_SECS
    if raw:
        try:
            parsed = float(raw)
            if parsed > 0:
                read = parsed
        except ValueError:
            pass
    return httpx.Timeout(
        connect=_DEFAULT_ASK_CONNECT_TIMEOUT_SECS,
        read=read,
        write=_DEFAULT_ASK_WRITE_TIMEOUT_SECS,
        pool=_DEFAULT_ASK_POOL_TIMEOUT_SECS,
    )


def resolve_ork_creds() -> tuple[str, str, str]:
    """Returns (base_url, jwt, tenant_id)."""
    base = _env("ORK_BASE_URL") or _env("BASE_URL") or "http://127.0.0.1:8080"
    jw = _env("ORK_JWT") or _env("JWT")
    tid = _env("ORK_TENANT_ID") or _env("TENANT_ID")
    if not jw or not tid:
        root = _env("DEMO_ROOT")
        if root:
            d = _load_dotenv_file(Path(root) / ".env")
            jw = jw or d.get("JWT", "")
            tid = tid or d.get("TENANT_ID", "")
    return base.rstrip("/"), jw, tid


def extract_reply_text_from_task_or_message(result: Any) -> str:
    if isinstance(result, dict) and "history" in result:
        return extract_reply_text_from_task_or_message(Task.model_validate(result))
    if isinstance(result, Task):
        for m in reversed(result.history):
            if m.role == Role.agent:
                for p in m.parts:
                    if isinstance(p, PartText):
                        return p.text
        return json.dumps(
            [x.model_dump(mode="json", exclude_none=True) for x in result.history]
        )[:8000]
    if isinstance(result, dict) and "parts" in result:
        m = Message.model_validate(result)
        for p in m.parts:
            if isinstance(p, PartText):
                return p.text
    if isinstance(result, Message):
        for p in result.parts:
            if isinstance(p, PartText):
                return p.text
    return str(result)[:8000]


def _task_id_from_a2a_result(result: Any) -> str | None:
    if isinstance(result, dict):
        tid = result.get("id")
        if isinstance(tid, str) and tid:
            return tid
    try:
        return str(Task.model_validate(result).id)
    except Exception:  # noqa: BLE001
        return None


async def ask_ork(agent_id: str, prompt: str, trace_id: str | None = None) -> str:
    """
    A2A `message/send` to ork: POST {base}/a2a/agents/{agent_id} with JSON-RPC body.
    """
    rid = trace_id or str(uuid.uuid4())
    base, jwt, tenant = resolve_ork_creds()
    if not jwt or not tenant:
        return (
            "ask_ork: missing JWT/TENANT_ID. Run `make -C demo demo-stage-0` to write "
            "demo/.env, set DEMO_ROOT to the demo/ path, or export ORK_JWT and ORK_TENANT_ID."
        )
    message_id = str(uuid.uuid4())
    body = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "role": "user",
                "parts": [{"kind": "text", "text": prompt}],
                "message_id": message_id,
                "task_id": None,
                "context_id": None,
                "metadata": None,
            }
        },
    }
    url = f"{base}/a2a/agents/{agent_id}"
    headers = {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {jwt}",
        "X-Tenant-Id": tenant,
    }
    async with httpx.AsyncClient(timeout=_resolve_ask_timeout()) as client:
        r = await client.post(url, json=body, headers=headers)
        r.raise_for_status()
        data = r.json()
    if data.get("error"):
        return f"ask_ork RPC error: {data['error']}"
    result = data.get("result")
    if result is None:
        return "ask_ork: no result in response"
    reply = extract_reply_text_from_task_or_message(result)
    task_uuid = _task_id_from_a2a_result(result)
    log.info(
        "[ask_ork] run=%s agent=%s task_id=%s reply_chars=%d",
        rid,
        agent_id,
        task_uuid or "unknown",
        len(reply),
    )
    return reply
