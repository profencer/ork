-- ADR-0053 §`Reference implementation` — three memory tables for the
-- Postgres-backed `MemoryStore`:
--   - `mem_messages`     — chat history per (tenant, resource, thread).
--   - `mem_working`      — durable per-resource working-memory state.
--   - `mem_embeddings`   — semantic-recall vector index (pgvector).
--
-- Tenant scoping is enforced at the schema level (every row carries
-- `tenant_id`) and at the row-level via RLS policies that read
-- `app.current_tenant_id`, mirroring `001_initial.sql` and the broader
-- ADR-0020 contract documented in `010_rls_policies.sql`. The
-- `ork-memory::postgres_backend::PgMemory` impl opens every read/write
-- inside a tenant-scoped transaction so these policies are load-bearing.

-- ADR-0053 §`Embedder selection`: dimension matches `text-embedding-3-small`
-- (1536). Customers that wire a different embedder via `Memory::postgres
-- (...).embedder(...)` should ALTER COLUMN ... TYPE VECTOR(N) before
-- writing — pgvector enforces the dimension at insert time.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE mem_messages (
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    resource_id  UUID NOT NULL,
    thread_id    UUID NOT NULL,
    agent_id     TEXT NOT NULL,
    message_id   UUID PRIMARY KEY,
    role         TEXT NOT NULL,
    content      TEXT NOT NULL,
    parts        JSONB NOT NULL DEFAULT '[]'::jsonb,
    tool_calls   JSONB NOT NULL DEFAULT '[]'::jsonb,
    tool_call_id TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_mem_messages_thread
    ON mem_messages (tenant_id, resource_id, thread_id, created_at DESC);

-- ADR-0053: working memory is keyed per-agent. The "shared across all
-- agents owned by `(tenant, resource)`" mode that Mastra's
-- `WorkingMemoryScope::Resource` describes is *deferred* — implementing
-- it cleanly in Postgres requires a partial-unique-index design that
-- the v1 schema is not paying for. Until that follow-up lands,
-- `agent_id` is `NOT NULL` and every agent gets its own row.
CREATE TABLE mem_working (
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    resource_id  UUID NOT NULL,
    agent_id     TEXT NOT NULL,
    value        JSONB NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, resource_id, agent_id)
);

CREATE TABLE mem_embeddings (
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    resource_id  UUID NOT NULL,
    thread_id    UUID NOT NULL,
    message_id   UUID PRIMARY KEY REFERENCES mem_messages(message_id) ON DELETE CASCADE,
    embedding    VECTOR(1536) NOT NULL,
    content      TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_mem_embeddings_resource
    ON mem_embeddings (tenant_id, resource_id);

-- Approximate-nearest-neighbor index for semantic recall. ivfflat needs an
-- ANALYZE pass to be effective; for v1 dev we accept the seq scan fallback
-- on small corpora and the index kicks in at production scale.
CREATE INDEX idx_mem_embeddings_cosine
    ON mem_embeddings USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);

-- Row-level security mirrors ADR-0020. Each table is scoped by tenant
-- so direct queries from a tenant-scoped Postgres role only see that
-- tenant's rows. `set_config('app.current_tenant_id', ..., true)` in
-- `ork-persistence::postgres::tenant_scope::open_tenant_tx` (and the
-- duplicated helper in `ork-memory::postgres_backend`) sets the GUC.

ALTER TABLE mem_messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE mem_working  ENABLE ROW LEVEL SECURITY;
ALTER TABLE mem_embeddings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_mem_messages ON mem_messages
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);

CREATE POLICY tenant_isolation_mem_working ON mem_working
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);

CREATE POLICY tenant_isolation_mem_embeddings ON mem_embeddings
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);
