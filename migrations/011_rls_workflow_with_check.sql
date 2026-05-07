-- ADR-0020 §`Mesh trust — JWT claims and propagation`: write-side RLS
-- enforcement for `workflow_definitions` and `workflow_runs`.
--
-- The original policies in `001_initial.sql:50-54` only carried `USING`,
-- which scopes the *qualifier* applied to reads / `UPDATE` `WHERE` clauses
-- but does NOT constrain `INSERT` / `UPDATE SET` row contents. Without
-- `WITH CHECK`, a session running under tenant A's `app.current_tenant_id`
-- could still `INSERT INTO workflow_runs (tenant_id, ...) VALUES
-- ($tenant_b, ...)` and the row would be persisted (just invisible to A's
-- subsequent reads). ADR-0020 demands write-side enforcement too — same
-- predicate as the read side — so we re-create both policies with a
-- `WITH CHECK` clause matching their `USING`.
--
-- `DROP POLICY IF EXISTS` then `CREATE POLICY` is preferred over the
-- (non-portable, version-dependent) `ALTER POLICY` form: we want the
-- migration to be re-runnable on any environment that may have applied a
-- subset of the predicates.

DROP POLICY IF EXISTS tenant_isolation_definitions ON workflow_definitions;
CREATE POLICY tenant_isolation_definitions ON workflow_definitions
    USING      (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);

DROP POLICY IF EXISTS tenant_isolation_runs ON workflow_runs;
CREATE POLICY tenant_isolation_runs ON workflow_runs
    USING      (tenant_id = current_setting('app.current_tenant_id')::UUID)
    WITH CHECK (tenant_id = current_setting('app.current_tenant_id')::UUID);
