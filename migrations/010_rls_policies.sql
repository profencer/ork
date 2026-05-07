-- ADR-0020 §`Mesh trust — JWT claims and propagation`: RLS policies for
-- tenant-scoped tables introduced after `001_initial.sql`. The
-- `current_setting('app.current_tenant_id')::UUID` GUC is bound by the
-- `open_tenant_tx` helper in `ork-persistence::postgres::tenant_scope`.
--
-- Each policy carries both `USING` (read/qualifier check) and `WITH CHECK`
-- (write check) so INSERTs are guarded by the same predicate as reads —
-- without `WITH CHECK`, a row could be inserted with a tenant_id that the
-- caller would then be unable to see.
--
-- Tables in scope:
--   - `webui_projects`        (introduced in 008_webui_projects.sql)
--   - `webui_conversations`   (introduced in 008_webui_projects.sql)
--
-- NOT in scope:
--   - `workflow_snapshots` (009): no `tenant_id` column; needs a
--     denormalisation migration before RLS can match the existing
--     `tenant_isolation_*` policy shape. Tracked as an ADR-0020 follow-up.
--   - `a2a_signing_keys` (005): KEK-protected, not tenant-scoped (ADR-0009).

ALTER TABLE webui_projects ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation_webui_projects ON webui_projects
    USING      (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);

ALTER TABLE webui_conversations ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation_webui_conversations ON webui_conversations
    USING      (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);

-- ADR-0020 §`Tenant CRUD restricted`: `tenants` is admin-managed across
-- tenants and gated by `tenant:admin` / `tenant:self` scope checks at the
-- HTTP layer (`crates/ork-api/src/routes/tenants.rs`). RLS was enabled on
-- the table in `001_initial.sql:45` but no policy was ever written, which
-- under any non-owner Postgres role results in *deny all* (a foot-gun for
-- production deployments using a non-superuser ork-api role). Disable RLS
-- on `tenants` here so the table is governed solely by the route-level
-- scope gate. If a future ADR wants RLS on `tenants` it must also write the
-- corresponding policy.
ALTER TABLE tenants DISABLE ROW LEVEL SECURITY;
