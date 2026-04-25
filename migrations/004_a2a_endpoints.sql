-- ADR-0008 A2A server endpoints: extend the slim `a2a_tasks` slice from
-- `003_delegation.sql` to the full task lifecycle and persist the per-task
-- message log; ADR-0009's `a2a_push_configs` table is pulled forward so the
-- JSON-RPC dispatcher can serve `tasks/pushNotificationConfig/*` end-to-end.
--
-- Migration numbering note: ADR-0008 references this as `002_a2a_tasks.sql`,
-- but `002_workflow_status_extensions.sql` and `003_delegation.sql` were
-- already merged. The slim `a2a_tasks` table from `003_delegation.sql` is the
-- starting point; this migration brings it up to the ADR-0008 target shape.

ALTER TABLE a2a_tasks
    ADD COLUMN context_id   UUID NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN metadata     JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN completed_at TIMESTAMPTZ NULL;

CREATE INDEX idx_a2a_tasks_context_id ON a2a_tasks (context_id);

CREATE TABLE a2a_messages (
    id          UUID PRIMARY KEY,
    task_id     UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
    role        TEXT NOT NULL,
    parts       JSONB NOT NULL,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    seq         BIGSERIAL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_a2a_messages_task_seq ON a2a_messages (task_id, seq);

ALTER TABLE a2a_messages ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_a2a_messages ON a2a_messages
    USING (
        task_id IN (
            SELECT id FROM a2a_tasks
            WHERE tenant_id = current_setting('app.current_tenant_id')::UUID
        )
    );

-- ADR-0009 push notification config (pulled forward; functional handlers ship
-- alongside this migration so the dispatcher can serve all six wire methods
-- end-to-end). Webhook delivery / push outbox worker remain ADR-0009 scope.
CREATE TABLE a2a_push_configs (
    id              UUID PRIMARY KEY,
    task_id         UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    url             TEXT NOT NULL,
    token           TEXT,
    authentication  JSONB,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_a2a_push_configs_task_id ON a2a_push_configs (task_id);
CREATE INDEX idx_a2a_push_configs_tenant  ON a2a_push_configs (tenant_id);

ALTER TABLE a2a_push_configs ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_a2a_push_configs ON a2a_push_configs
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);

COMMENT ON COLUMN a2a_messages.role IS
  'A2A Message.role wire form: "user" | "agent" (matches ork_a2a::Role serde).';
COMMENT ON COLUMN a2a_tasks.context_id IS
  'A2A Task.context_id (ADR-0008): groups related tasks under one conversation.';
COMMENT ON COLUMN a2a_tasks.completed_at IS
  'Set when state moves to a terminal value (completed | failed | canceled | rejected).';
