-- ADR-0016: artifact metadata index. The ADR text referenced `004_artifacts.sql`;
-- `004_a2a_endpoints.sql` is already in tree, and later migrations are 005, 006 — this is 007.
--
-- `context_id` is NOT NULL: tenant-global artifacts use the nil UUID (same as
-- `ork_core::ports::artifact_store::NO_CONTEXT_ID` and ADR-0016 `COALESCE`).

CREATE TABLE artifacts (
    tenant_id   UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    context_id  UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
    name        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    scheme      TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    mime        TEXT,
    size        BIGINT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by  TEXT,
    task_id     UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
    labels      JSONB NOT NULL DEFAULT '{}'::jsonb,
    etag        TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (tenant_id, context_id, name, version)
);

CREATE INDEX artifacts_context_idx ON artifacts(context_id);
CREATE INDEX artifacts_task_id_idx ON artifacts(task_id);
CREATE INDEX artifacts_tenant_name_idx ON artifacts(tenant_id, name);

ALTER TABLE artifacts ENABLE ROW LEVEL SECURITY;

CREATE POLICY artifacts_tenant_isolation ON artifacts
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);

COMMENT ON TABLE artifacts IS 'ADR-0016: metadata index; blobs live in ArtifactStore.';
