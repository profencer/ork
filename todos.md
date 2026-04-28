ReAct-style iterative tool use inside a step (research would benefit, but the engine's one-shot model is workable with a good code_search + read_file pre-fetch).

Test on Deepseek V4 in reasoning mode, it needs resoning block in response

https://github.com/lmnr-ai/lmnr(observability adr)


(https://hindsight.vectorize.io)
Agent memory / store. LangGraph has first-class cross-thread memory, a Store interface, semantic search over message history, and a clean short-term/long-term split. I don't see an ADR for any of this in the index. MCP can serve it, but a portable memory port belongs in ork-agents.

Durable execution & time-travel. LangGraph 0.2+ has crash-safe per-node checkpointing and the ability to fork a run from any past checkpoint. 0022 gives you a task event log; you don't yet have replay-from-checkpoint as a contract.

Dynamic fan-out (Send API). LangGraph's Send lets a node emit N child invocations with per-child state at runtime. 0018 is where this would live — check it covers map/reduce, not just static DAG.

Streaming modes. LangGraph exposes values | updates | messages | custom | debug stream channels. SSE alone isn't equivalent — you need the channel taxonomy too, otherwise UI clients have to sniff event shapes.

Eval / regression harness. LangSmith's dataset + evaluator + replay loop is a real moat for enterprise adoption. Nothing in the ADR set addresses "how do I prove this workflow didn't regress."

Production maturity, not features. LangGraph has thousands of prod deployments and the bug reports that come with them. ork has zero. That's not a feature gap, but it's the gap an enterprise buyer cares about most.