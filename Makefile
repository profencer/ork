# Root Makefile — only contains demo shims so the rest of the workspace stays
# `cargo`-driven. Real targets live in `demo/Makefile`.

.PHONY: demo demo-up demo-down demo-help

demo: ## Run the full demo (stages 0–7, 9, 8) end-to-end.
	@$(MAKE) -C demo demo-all

demo-up: ## Boot the demo infra (compose + ork-api + helpers) without running stages.
	@$(MAKE) -C demo demo-up

demo-down: ## Tear the demo down: kill background PIDs + compose down -v.
	@$(MAKE) -C demo demo-down

demo-help: ## Show the per-stage demo targets.
	@$(MAKE) -C demo help
