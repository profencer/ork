ReAct-style iterative tool use inside a step (research would benefit, but the engine's one-shot model is workable with a good code_search + read_file pre-fetch).



Parallel execution of fan-out branches — start sequential; add tokio::join_all later if latency matters.



Embedding / AST indexing — ripgrep is enough for v1.