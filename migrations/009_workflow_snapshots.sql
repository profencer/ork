-- ADR-0050: suspend/resume snapshots for code-first workflows.

CREATE TABLE workflow_snapshots (
    workflow_id TEXT NOT NULL,
    run_id UUID NOT NULL,
    step_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    payload JSONB NOT NULL,
    resume_schema JSONB NOT NULL,
    run_state JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    consumed_at TIMESTAMPTZ,
    CONSTRAINT workflow_snapshots_unique UNIQUE (workflow_id, run_id, step_id, attempt)
);

CREATE INDEX workflow_snapshots_pending_idx
    ON workflow_snapshots (workflow_id, run_id)
    WHERE consumed_at IS NULL;
