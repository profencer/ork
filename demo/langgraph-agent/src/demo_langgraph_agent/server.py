"""Starlette: AgentCard + JSON-RPC (message/send, message/stream) with ork-compatible SSE."""

from __future__ import annotations

import asyncio
import json
import logging
import uuid
from collections.abc import AsyncIterator
from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse, Response, StreamingResponse
from starlette.routing import Route

from demo_langgraph_agent.a2a_types import (
    AgentCard,
    JsonRpcId,
    JsonRpcRequestGeneric,
    Message,
    MessageSendParams,
    PartText,
    Role,
    Task,
    TaskState,
    TaskStatus,
    TaskStatusUpdateEvent,
    jsonrpc_error,
    sse_task_event_dict,
)
from demo_langgraph_agent.agent import run_research_session

log = logging.getLogger(__name__)

METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603


def new_id() -> str:
    return str(uuid.uuid4())


def first_user_text(m: Message) -> str:
    from demo_langgraph_agent.a2a_types import PartText

    for p in m.parts:
        if isinstance(p, PartText):
            return p.text
    return ""


def build_task_from_roundtrip(
    task_id: str,
    context_id: str,
    user: Message,
    agent_reply: str,
) -> Task:
    agent_msg = Message(
        role=Role.agent,
        parts=[PartText(text=agent_reply)],
        message_id=new_id(),
        task_id=task_id,
        context_id=context_id,
    )
    return Task(
        id=task_id,
        context_id=context_id,
        status=TaskStatus(state=TaskState.completed, message="done"),
        history=[user, agent_msg],
        artifacts=[],
    )


def format_sse(d: dict[str, Any]) -> str:
    return f"data: {json.dumps(d, separators=(',', ':'))}\n\n"


async def _stream_events(params: MessageSendParams) -> AsyncIterator[bytes]:
    m = params.message
    user_text = first_user_text(m)
    task_id = m.task_id or new_id()
    context_id = m.context_id or new_id()
    ev0 = TaskStatusUpdateEvent(
        task_id=task_id,
        status=TaskStatus(state=TaskState.working, message="thinking"),
        is_final=False,
    )
    yield format_sse(sse_task_event_dict(ev0)).encode()

    # Keep the HTTP+SSE body alive: ork applies an idle timeout between parsed events.
    # ReAct (LLM + ask_ork) can run longer than 60s with no other chunks without this.
    _heartbeat_s = 25.0

    try:
        job = asyncio.create_task(run_research_session(user_text))
        while True:
            try:
                reply = await asyncio.wait_for(asyncio.shield(job), timeout=_heartbeat_s)
                break
            except TimeoutError:
                ev_hb = TaskStatusUpdateEvent(
                    task_id=task_id,
                    status=TaskStatus(
                        state=TaskState.working, message="processing (LLM or tools)…"
                    ),
                    is_final=False,
                )
                yield format_sse(sse_task_event_dict(ev_hb)).encode()
    except Exception as e:
        log.exception("graph failed")
        ev_err = TaskStatusUpdateEvent(
            task_id=task_id,
            status=TaskStatus(state=TaskState.failed, message=str(e)[:500]),
            is_final=True,
        )
        yield format_sse(sse_task_event_dict(ev_err)).encode()
        return

    amsg = Message(
        role=Role.agent,
        parts=[PartText(text=reply)],
        message_id=new_id(),
        task_id=task_id,
        context_id=context_id,
    )
    yield format_sse(sse_task_event_dict(amsg)).encode()

    evf = TaskStatusUpdateEvent(
        task_id=task_id,
        status=TaskStatus(state=TaskState.completed, message="done"),
        is_final=True,
    )
    yield format_sse(sse_task_event_dict(evf)).encode()


def make_rpc_handler(card: AgentCard) -> Any:
    async def get_card(_: Request) -> JSONResponse:
        return JSONResponse(
            content=card.model_dump(mode="json", exclude_none=True),
        )

    async def rpc_root(request: Request) -> Response:
        try:
            body = await request.json()
        except Exception as e:  # noqa: BLE001
            return JSONResponse(
                content=jsonrpc_error(None, -32700, f"parse error: {e}"),
                status_code=200,
            )
        if not isinstance(body, dict):
            return JSONResponse(
                content=jsonrpc_error(None, -32600, "body must be a JSON object"),
                status_code=200,
            )
        req_id: JsonRpcId = body.get("id")
        try:
            req = JsonRpcRequestGeneric.model_validate(body)
        except Exception as e:  # noqa: BLE001
            return JSONResponse(
                content=jsonrpc_error(req_id, -32600, str(e)),
                status_code=200,
            )
        if req.jsonrpc != "2.0":
            return JSONResponse(
                content=jsonrpc_error(req_id, -32600, 'jsonrpc must be "2.0"'),
                status_code=200,
            )
        if req.params is None:
            return JSONResponse(
                content=jsonrpc_error(req_id, INVALID_PARAMS, "missing params"),
                status_code=200,
            )
        try:
            p = MessageSendParams.model_validate(req.params)
        except Exception as e:  # noqa: BLE001
            return JSONResponse(
                content=jsonrpc_error(req_id, INVALID_PARAMS, str(e)), status_code=200
            )
        if req.method == "message/stream":
            return StreamingResponse(_stream_events(p), media_type="text/event-stream")
        if req.method == "message/send":
            m = p.message
            try:
                reply = await run_research_session(first_user_text(m))
            except Exception as e:  # noqa: BLE001
                log.exception("message/send")
                return JSONResponse(
                    content=jsonrpc_error(req_id, INTERNAL_ERROR, str(e)), status_code=200
                )
            task_id = m.task_id or new_id()
            context_id = m.context_id or new_id()
            u = m.model_copy(
                update={
                    "message_id": m.message_id or new_id(),
                    "task_id": task_id,
                    "context_id": context_id,
                },
                deep=True,
            )
            task = build_task_from_roundtrip(task_id, context_id, u, reply)
            return JSONResponse(
                {
                    "jsonrpc": "2.0",
                    "id": req.id,
                    "result": task.model_dump(mode="json", exclude_none=True),
                }
            )
        return JSONResponse(
            content=jsonrpc_error(req_id, METHOD_NOT_FOUND, f"Method not found: {req.method}"),
            status_code=200,
        )

    return Starlette(
        debug=False,
        routes=[
            Route("/.well-known/agent-card.json", get_card, methods=["GET"]),
            Route("/", rpc_root, methods=["POST"]),
        ],
    )
