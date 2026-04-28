"""Unit tests for `demo_langgraph_agent.ork_client` helpers."""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any

import httpx
import pytest

from demo_langgraph_agent.ork_client import (
    _DEFAULT_ASK_CONNECT_TIMEOUT_SECS,
    _DEFAULT_ASK_POOL_TIMEOUT_SECS,
    _DEFAULT_ASK_READ_TIMEOUT_SECS,
    _DEFAULT_ASK_WRITE_TIMEOUT_SECS,
    _resolve_ask_timeout,
)

# Preserve real client class; patching `ork_client.httpx.AsyncClient` must not recurse here.
_HttpxAsyncClient = httpx.AsyncClient


def test_resolve_ask_timeout_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ORK_ASK_TIMEOUT_SECS", raising=False)
    t = _resolve_ask_timeout()
    assert t.read == _DEFAULT_ASK_READ_TIMEOUT_SECS
    assert t.connect == _DEFAULT_ASK_CONNECT_TIMEOUT_SECS
    assert t.write == _DEFAULT_ASK_WRITE_TIMEOUT_SECS
    assert t.pool == _DEFAULT_ASK_POOL_TIMEOUT_SECS


def test_resolve_ask_timeout_env_override(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ORK_ASK_TIMEOUT_SECS", "45")
    t = _resolve_ask_timeout()
    assert t.read == 45.0
    # only the read phase is overridable; the others stay at the safe defaults
    assert t.connect == _DEFAULT_ASK_CONNECT_TIMEOUT_SECS
    assert t.write == _DEFAULT_ASK_WRITE_TIMEOUT_SECS
    assert t.pool == _DEFAULT_ASK_POOL_TIMEOUT_SECS


def test_resolve_ask_timeout_invalid_env_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ORK_ASK_TIMEOUT_SECS", "not-a-number")
    t = _resolve_ask_timeout()
    assert t.read == _DEFAULT_ASK_READ_TIMEOUT_SECS


def test_resolve_ask_timeout_non_positive_env_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ORK_ASK_TIMEOUT_SECS", "0")
    t = _resolve_ask_timeout()
    assert t.read == _DEFAULT_ASK_READ_TIMEOUT_SECS


def test_ask_ork_trace_id_and_task_logged(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.setenv("ORK_JWT", "jwt-test")
    monkeypatch.setenv("ORK_TENANT_ID", "00000000-0000-0000-0000-000000000099")
    monkeypatch.setenv("ORK_BASE_URL", "http://test.ork")

    captured: dict[str, Any] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["url"] = str(request.url)
        captured["body"] = json.loads(request.content.decode())
        return httpx.Response(
            200,
            json={
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "id": "019dd3a5-cc7f-7812-aac6-aa721a87546d",
                    "context_id": "00000000-0000-0000-0000-000000000002",
                    "status": {"state": "completed"},
                    "history": [
                        {
                            "role": "user",
                            "message_id": "m-user",
                            "parts": [{"kind": "text", "text": "q"}],
                        },
                        {
                            "role": "agent",
                            "message_id": "m-agent",
                            "parts": [{"kind": "text", "text": "researcher reply body"}],
                        },
                    ],
                },
            },
        )

    transport = httpx.MockTransport(handler)

    def make_client(**kwargs: Any) -> httpx.AsyncClient:
        return _HttpxAsyncClient(
            transport=transport,
            timeout=kwargs.get("timeout", httpx.Timeout(30.0)),
        )

    monkeypatch.setattr("demo_langgraph_agent.ork_client.httpx.AsyncClient", make_client)

    async def _run() -> str:
        from demo_langgraph_agent.ork_client import ask_ork

        return await ask_ork("researcher", "prompt text", trace_id="trace-fixed")

    with caplog.at_level(logging.INFO, logger="demo_langgraph_agent.ork_client"):
        out = asyncio.run(_run())

    assert out == "researcher reply body"
    assert captured["url"] == "http://test.ork/a2a/agents/researcher"
    body = captured["body"]
    assert body["method"] == "message/send"
    assert body["params"]["message"]["parts"][0]["text"] == "prompt text"
    assert "run=trace-fixed" in caplog.text
    assert "task_id=019dd3a5-cc7f-7812-aac6-aa721a87546d" in caplog.text
    assert "reply_chars=" in caplog.text
