"""Call back into ork's A2A JSON-RPC as a client (reverse leg of the demo)."""

from __future__ import annotations

import json
import re
import uuid
from pathlib import Path
from typing import Any

import httpx

from demo_langgraph_agent.a2a_types import Message, PartText, Role, Task


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


async def ask_ork(agent_id: str, prompt: str) -> str:
    """
    A2A `message/send` to ork: POST {base}/a2a/agents/{agent_id} with JSON-RPC body.
    """
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
    async with httpx.AsyncClient(timeout=httpx.Timeout(120.0)) as client:
        r = await client.post(url, json=body, headers=headers)
        r.raise_for_status()
        data = r.json()
    if data.get("error"):
        return f"ask_ork RPC error: {data['error']}"
    result = data.get("result")
    if result is None:
        return "ask_ork: no result in response"
    return extract_reply_text_from_task_or_message(result)
