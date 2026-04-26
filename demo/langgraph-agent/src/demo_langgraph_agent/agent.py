"""LangGraph: ReAct loop with an `ask_ork` tool that calls ork's local agents via A2A."""

from __future__ import annotations

import os
from collections.abc import Sequence
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

    return await _ask(agent_id, prompt)


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


def last_text_content(messages: Sequence[BaseMessage]) -> str:
    for m in reversed(list(messages)):
        if isinstance(m, AIMessage) and m.content and not m.tool_calls:
            if isinstance(m.content, str):
                return m.content
    for m in reversed(list(messages)):
        if isinstance(m, ToolMessage) and m.content:
            if isinstance(m.content, str) and m.content:
                return m.content
    return "No final assistant text."


async def run_research_session(user_text: str) -> str:
    """Run one graph turn: system + user, until no tool calls."""
    graph = build_graph()
    initial: list[BaseMessage] = [SystemMessage(content=SYSTEM), HumanMessage(content=user_text)]
    st = await graph.ainvoke({"messages": initial})
    return last_text_content(st["messages"])
