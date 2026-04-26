"""CLI: `python -m demo_langgraph_agent --addr 127.0.0.1:8092`"""

from __future__ import annotations

import argparse
import logging
import os
import sys

import uvicorn

from demo_langgraph_agent.a2a_types import AgentCapabilities, AgentCard, AgentSkill
from demo_langgraph_agent.server import make_rpc_handler

log = logging.getLogger(__name__)


def build_card(public_base: str) -> AgentCard:
    b = public_base.rstrip("/") + "/"
    return AgentCard(
        name="LangGraph Research Peer (ork demo)",
        description=(
            "LangGraph agent that delegates repository questions to ork's researcher "
            "via A2A (bidirectional demo)."
        ),
        version="0.1.0",
        url=b,
        provider=None,
        capabilities=AgentCapabilities(
            streaming=True,
            push_notifications=False,
            state_transition_history=False,
        ),
        default_input_modes=["text"],
        default_output_modes=["text"],
        skills=[
            AgentSkill(
                id="research",
                name="Repository research (via ork)",
                description=(
                    "Uses ask_ork(researcher) for list_repos, code_search, read_file, then answers."
                ),
                tags=["langgraph", "a2a", "researcher"],
                examples=["Where is the HTTP client in anthropic-sdk-typescript?"],
            )
        ],
        security_schemes=None,
        security=None,
        extensions=None,
    )


def main() -> None:
    logging.basicConfig(
        level=os.environ.get("LOG_LEVEL", "INFO"),
        format="%(message)s",
    )
    p = argparse.ArgumentParser(description="A2A LangGraph demo peer for ork")
    p.add_argument(
        "--addr",
        default=os.environ.get("LG_ADDR", "127.0.0.1:8092"),
        help="Listen address (host:port) — also used as AgentCard url root",
    )
    p.add_argument(
        "--card-url",
        default=os.environ.get("LG_PUBLIC_BASE", ""),
        help="Override AgentCard url (e.g. http://127.0.0.1:8092/); default http://<addr>/",
    )
    args = p.parse_args()
    if ":" not in args.addr:
        log.error("expected --addr host:port, got %r", args.addr)
        sys.exit(2)
    host, port_s = args.addr.rsplit(":", 1)
    try:
        port = int(port_s)
    except ValueError:
        log.error("bad port in --addr: %r", args.addr)
        sys.exit(2)
    public = args.card_url.strip() or f"http://{args.addr}/"
    card = build_card(public)
    app = make_rpc_handler(card)
    log.info(
        "LangGraph A2A peer on http://%s  (card %s/.well-known/agent-card.json)",
        args.addr,
        public.rstrip("/"),
    )
    uvicorn.run(
        app,
        host=host,
        port=port,
        log_level=os.environ.get("UVICORN_LOG_LEVEL", "info"),
        access_log=False,
    )


if __name__ == "__main__":
    main()
