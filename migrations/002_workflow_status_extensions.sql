-- Document extended workflow run statuses (ADR 0003). Values are stored as TEXT; no schema change required.
COMMENT ON COLUMN workflow_runs.status IS
  'Workflow run lifecycle: pending | running | input_required | auth_required | completed | failed | cancelled | rejected';
