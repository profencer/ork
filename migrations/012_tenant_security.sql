-- ADR-0020 §`Secrets handling`: per-tenant envelope encryption columns.
--
-- Each tenant gets a randomly-generated 32-byte DEK; the DEK is wrapped
-- under the KMS-managed KEK (legacy: HKDF-derived from `auth.jwt_secret`)
-- and persisted alongside its tenant. Per-field `*_encrypted` columns
-- inside `tenants.settings` are AES-GCM-sealed under the tenant's DEK.
--
-- See `crates/ork-security/src/{kms,tenant_cipher}.rs` for the in-process
-- cipher. The pre-migration shim in `crates/ork-persistence/src/postgres/
-- tenant_repo.rs` lazily mints a DEK on first read of any tenant whose
-- `dek_wrapped IS NULL` (i.e. existing rows from before this migration).

ALTER TABLE tenants
    ADD COLUMN dek_wrapped BYTEA,
    ADD COLUMN dek_key_id TEXT,
    ADD COLUMN dek_version TEXT;

-- `dek_wrapped` is NULL for rows created before this migration. Once the
-- one-time shim runs, every row gains a wrapped DEK; we *don't* enforce
-- NOT NULL here so the rolling migration can drain in production without
-- a synchronised flush.
COMMENT ON COLUMN tenants.dek_wrapped IS
    'Wrapped DEK bytes (KMS-side ciphertext). NULL on pre-ADR-0020 rows; \
     populated by the tenant_repo migration shim on first access.';
COMMENT ON COLUMN tenants.dek_key_id IS
    'Provider-side KEK identifier (e.g. AWS KMS ARN). NULL for the legacy \
     adapter where the KEK is implicit (HKDF from auth.jwt_secret).';
COMMENT ON COLUMN tenants.dek_version IS
    'On-wire version tag for the wrapped-DEK byte layout. Currently "v1".';
