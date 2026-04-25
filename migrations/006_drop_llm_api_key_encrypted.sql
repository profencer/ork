-- ADR 0012 (OpenAI-compatible LLM provider catalog).
--
-- `TenantSettings.llm_api_key_encrypted` is removed without a back-compat
-- shim — every per-tenant secret now lives inside the per-provider
-- `headers` map of the new `llm_providers` catalog (ADR 0012
-- §`Tenant overrides`). The repo is pre-1.0 with no production tenants;
-- the ADR justifies the no-shim choice in `## Alternatives considered`.
--
-- We drop the dead key from the JSONB blob in a single UPDATE so the
-- shape on disk matches the Rust struct after the deploy. Rows that
-- never had the key still parse cleanly via `serde(default)` on the
-- new fields.

UPDATE tenants
SET    settings = settings - 'llm_api_key_encrypted'
WHERE  settings ? 'llm_api_key_encrypted';
