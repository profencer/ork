-- Peer delegation linkage (ADR 0006).
--
-- Adds parent_run_id / parent_step_id / parent_task_id to workflow_runs so a child
-- WorkflowRun forked by a `delegate_to` step (or a `child_workflow` delegation) can
-- be traced back to the parent run and step. Adds a minimal `a2a_tasks` table that
-- ADR 0008 will extend with messages and a state log; the slice below is the subset
-- ADR 0006 needs for parent_task_id linkage and `await:false` task tracking.

ALTER TABLE workflow_runs
    ADD COLUMN parent_run_id  UUID NULL REFERENCES workflow_runs(id) ON DELETE SET NULL,
    ADD COLUMN parent_step_id TEXT NULL,
    ADD COLUMN parent_task_id UUID NULL;

CREATE INDEX idx_workflow_runs_parent_run ON workflow_runs (parent_run_id);

CREATE TABLE a2a_tasks (
    id              UUID PRIMARY KEY,
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id        TEXT NOT NULL,
    parent_task_id  UUID NULL REFERENCES a2a_tasks(id) ON DELETE SET NULL,
    workflow_run_id UUID NULL REFERENCES workflow_runs(id) ON DELETE SET NULL,
    state           TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_a2a_tasks_tenant      ON a2a_tasks (tenant_id);
CREATE INDEX idx_a2a_tasks_parent_task ON a2a_tasks (parent_task_id);
CREATE INDEX idx_a2a_tasks_workflow_run ON a2a_tasks (workflow_run_id);
CREATE INDEX idx_a2a_tasks_agent_state ON a2a_tasks (agent_id, state);

ALTER TABLE a2a_tasks ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_a2a_tasks ON a2a_tasks
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);

COMMENT ON COLUMN a2a_tasks.state IS
  'A2A task lifecycle (mirrors ork_a2a::TaskState): submitted | working | input_required | auth_required | completed | failed | canceled | rejected';
