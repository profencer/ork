-- ADR-0054 §`scorer_results table` — single Postgres-backed store for
-- both live-sampled and offline-replay scorer results. Studio (ADR
-- 0055) renders the dashboard from this table; the `ork eval` CLI
-- reads it for the `--baseline`/regression flow; CI gates against it
-- via `--fail-on score_below ...`.
--
-- Tenant scoping mirrors ADR-0020 (RLS policy reads
-- `app.current_tenant_id` GUC). Either `agent_id` or `workflow_id`
-- must be present per row — the `CHECK` constraint enforces that no
-- caller writes a row with both `NULL` (which would orphan the
-- result from any registered surface).

CREATE TABLE scorer_results (
    id                  UUID PRIMARY KEY,
    tenant_id           UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id            TEXT,
    workflow_id         TEXT,
    run_id              UUID NOT NULL,
    run_kind            TEXT NOT NULL,
    scorer_id           TEXT NOT NULL,
    score               REAL NOT NULL,
    label               TEXT,
    rationale           TEXT,
    details             JSONB NOT NULL DEFAULT '{}'::jsonb,
    scorer_duration_ms  INTEGER,
    judge_model         TEXT,
    judge_input_tokens  INTEGER,
    judge_output_tokens INTEGER,
    sampled_via         TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT scorer_results_target_present
        CHECK (agent_id IS NOT NULL OR workflow_id IS NOT NULL),
    CONSTRAINT scorer_results_run_kind_valid
        CHECK (run_kind IN ('agent', 'workflow'))
);

-- ADR-0054 acceptance criterion: indices on
-- `(tenant_id, agent_id, created_at DESC)` (Studio's per-agent
-- timeline) and `(tenant_id, scorer_id, score)` (regression detector
-- queries).
CREATE INDEX scorer_results_tenant_agent_created
    ON scorer_results (tenant_id, agent_id, created_at DESC);

CREATE INDEX scorer_results_tenant_scorer_score
    ON scorer_results (tenant_id, scorer_id, score);

-- Workflow timeline. Mirrors the agent index for `ScorerTarget::Workflow`
-- bindings (ADR-0054 user-confirmed: workflow live scoring ships in v1).
CREATE INDEX scorer_results_tenant_workflow_created
    ON scorer_results (tenant_id, workflow_id, created_at DESC);

-- ADR-0020 RLS policy: every read/write is gated on
-- `app.current_tenant_id`. `ork-persistence::postgres::tenant_scope`
-- sets the GUC inside the tenant-scoped transaction.
ALTER TABLE scorer_results ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_scorer_results ON scorer_results
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);
