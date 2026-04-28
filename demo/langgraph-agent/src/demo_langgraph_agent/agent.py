"""LangGraph: ReAct loop with an `ask_ork` tool that calls ork's local agents via A2A."""

from __future__ import annotations

import json
import logging
import os
import uuid
from collections.abc import Sequence
from contextvars import ContextVar
from typing import Any

from langchain_core.messages import (
    AIMessage,
    BaseMessage,
    HumanMessage,
    SystemMessage,
    ToolMessage,
)
from langchain_core.tools import tool
from langchain_openai import ChatOpenAI
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode

log = logging.getLogger(__name__)

# Set for the duration of `run_research_session` so `ask_ork` can tag logs for stage 9.
_langgraph_trace_run_id: ContextVar[str | None] = ContextVar("langgraph_trace_run_id", default=None)

_TRACE_CONTENT_MAX = 800


def _trace_truncate(s: str, max_len: int = _TRACE_CONTENT_MAX) -> str:
    if len(s) <= max_len:
        return s
    return f"{s[: max_len - 3]}..."


def _log_trace_agent(rid: str, msg: AIMessage) -> None:
    if msg.tool_calls:
        parts: list[str] = []
        for tc in msg.tool_calls:
            if isinstance(tc, dict):
                name = tc.get("name", "?")
                raw_args = tc.get("args")
                if raw_args is None:
                    raw_args = tc.get("arguments")
            else:
                name = getattr(tc, "name", "?")
                raw_args = getattr(tc, "args", None)
            args_s = (
                json.dumps(raw_args, ensure_ascii=False)
                if isinstance(raw_args, dict)
                else str(raw_args)
            )
            parts.append(f"{name}({args_s[:500]})")
        log.info("[trace agent] run=%s tool_calls=[%s]", rid, ", ".join(parts))
    else:
        text = _aimessage_text(msg)
        log.info("[trace agent] run=%s final_text=%s", rid, _trace_truncate(text))


def _log_trace_tools(rid: str, msg: ToolMessage) -> None:
    name = msg.name or "tool"
    content = msg.content if isinstance(msg.content, str) else str(msg.content)
    log.info("[trace tools] run=%s %s -> %s", rid, name, _trace_truncate(content))


SYSTEM = """You are a code-research router for the ork demo.
You MUST use the `ask_ork` tool to delegate repository research to ork's built-in
`researcher` agent (it has list_repos, code_search, read_file, etc. on the demo workspace).

1. Set agent_id to "researcher" (exactly).
2. In your prompt, ask a concrete question about the `anthropic-sdk-typescript` repository
   or the file layout (so code_search can return real hits).
3. When you receive tool output, reply with ONE short paragraph summarising the answer for
   the user. Do not invent file paths; prefer what the tool returned."""


@tool
async def ask_ork(agent_id: str, prompt: str) -> str:
    """
    Call an ork local agent (use agent_id "researcher") via A2A `message/send`.
    The prompt should ask for repo facts (files, search hits) the researcher can look up.
    """
    from demo_langgraph_agent.ork_client import ask_ork as _ask

    tid = _langgraph_trace_run_id.get()
    return await _ask(agent_id, prompt, trace_id=tid)


def _get_llm() -> ChatOpenAI:
    api_key = os.environ.get("OPENAI_API_KEY", "")
    if not api_key and os.environ.get("MINIMAX_API_KEY", ""):
        mk = os.environ["MINIMAX_API_KEY"]
        api_key = mk.removeprefix("Bearer ").strip() if mk.startswith("Bearer") else mk
    return ChatOpenAI(
        model=os.environ.get("OPENAI_MODEL", "MiniMax-M2.7"),
        base_url=os.environ.get("OPENAI_BASE_URL", "https://api.minimax.io/v1"),
        api_key=api_key,
        temperature=0.2,
    )


def _should_continue(state: MessagesState) -> str:
    last = state["messages"][-1]
    if isinstance(last, AIMessage) and last.tool_calls:
        return "tools"
    return "end"


def build_graph() -> Any:
    tools: Sequence[Any] = [ask_ork]
    llm = _get_llm().bind_tools(tools)
    tool_node = ToolNode(list(tools))

    async def call_model(state: MessagesState) -> dict[str, list[BaseMessage]]:
        m = state["messages"]
        resp = await llm.ainvoke(m)
        return {"messages": [resp]}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", call_model)
    builder.add_node("tools", tool_node)
    builder.add_edge(START, "agent")
    builder.add_conditional_edges("agent", _should_continue, {"tools": "tools", "end": END})
    builder.add_edge("tools", "agent")
    return builder.compile()


def _aimessage_text(msg: AIMessage) -> str:
    """Normalize content: some OpenAI-compatible APIs return list blocks, not str."""
    c = msg.content
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        parts: list[str] = []
        for block in c:
            if isinstance(block, str):
                parts.append(block)
            elif isinstance(block, dict):
                if block.get("type") == "text" and "text" in block:
                    parts.append(str(block["text"]))
                elif "text" in block:
                    parts.append(str(block["text"]))
            elif hasattr(block, "text"):
                parts.append(str(getattr(block, "text", "")))
        return "".join(parts)
    return str(c) if c else ""


def last_text_content(messages: Sequence[BaseMessage]) -> str:
    for m in reversed(list(messages)):
        if isinstance(m, AIMessage) and not m.tool_calls:
            text = _aimessage_text(m).strip()
            if text:
                return text
    for m in reversed(list(messages)):
        if isinstance(m, ToolMessage) and m.content:
            if isinstance(m.content, str) and m.content:
                return m.content
    return "No final assistant text."


async def run_research_session(user_text: str) -> str:
    """Run one graph turn: system + user, until no tool calls."""
    graph = build_graph()
    initial: list[BaseMessage] = [SystemMessage(content=SYSTEM), HumanMessage(content=user_text)]
    rid = str(uuid.uuid4())
    token = _langgraph_trace_run_id.set(rid)
    all_messages: list[BaseMessage] = list(initial)
    try:
        async for update in graph.astream({"messages": initial}, stream_mode="updates"):
            for _node_name, node_data in update.items():
                for m in node_data.get("messages", []):
                    all_messages.append(m)
                    if isinstance(m, AIMessage):
                        _log_trace_agent(rid, m)
                    elif isinstance(m, ToolMessage):
                        _log_trace_tools(rid, m)
    finally:
        _langgraph_trace_run_id.reset(token)
    return last_text_content(all_messages)
