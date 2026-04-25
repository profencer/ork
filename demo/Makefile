# demo/Makefile — orchestrates the 8-stage ork demo.
#
# Each stage is a self-contained shell script under demo/scripts/. A stage
# can be re-run independently as long as the previous stages have set up
# their state in demo/.env / demo/logs / Postgres.

SHELL := /usr/bin/env bash

DEMO_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
SCRIPTS := $(DEMO_DIR)/scripts

.PHONY: demo-all demo-up demo-down help \
        demo-stage-0 demo-stage-1 demo-stage-2 demo-stage-3 demo-stage-4 \
        demo-stage-5 demo-stage-6 demo-stage-7 demo-stage-8

help: ## Show every demo target with its description.
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

demo-all: demo-stage-0 demo-stage-1 demo-stage-2 demo-stage-3 demo-stage-4 demo-stage-5 demo-stage-6 demo-stage-7 demo-stage-8 ## Run every stage in order, end-to-end.

demo-up: demo-stage-0 ## Boot infra + ork-api so other stages can be run individually.

demo-down: demo-stage-8 ## Tear infra + background PIDs down (alias for stage 8).

demo-stage-0: ## Stage 0 — Bootstrap (compose, migrations, tenant, JWT, ork-api boot).
	@bash $(SCRIPTS)/stage-0-bootstrap.sh

demo-stage-1: ## Stage 1 — `ork standup` proves CLI + integrations (no LLM needed).
	@bash $(SCRIPTS)/stage-1-standup.sh

demo-stage-2: ## Stage 2 — Agent cards, `/a2a/agents`, + ADR-0013 gateway smokes.
	@bash $(SCRIPTS)/stage-2-cards.sh

demo-stage-3: ## Stage 3 — `message/stream` + SSE replay reconnect (ADR 0008).
	@bash $(SCRIPTS)/stage-3-stream.sh

demo-stage-4: ## Stage 4 — Multi-step workflow run (planner → researcher → synthesizer → writer → reviewer).
	@bash $(SCRIPTS)/stage-4-workflow.sh

demo-stage-5: ## Stage 5 — MCP tool call routed via CompositeToolExecutor (ADR 0010).
	@bash $(SCRIPTS)/stage-5-mcp.sh

demo-stage-6: ## Stage 6 — Federation: ork delegates to a stub vendor agent (ADR 0007).
	@bash $(SCRIPTS)/stage-6-federation.sh

demo-stage-7: ## Stage 7 — Push notifications + JWKS key rotation (ADR 0009).
	@bash $(SCRIPTS)/stage-7-push.sh

demo-stage-8: ## Stage 8 — Teardown (kill PIDs, compose down, clean state).
	@bash $(SCRIPTS)/stage-8-teardown.sh
