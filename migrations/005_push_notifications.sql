-- ADR-0009 push notifications: signing keys + dead-letter ledger.
--
-- Companion to `004_a2a_endpoints.sql` which already defines `a2a_push_configs`.
-- This migration adds:
--
--   * `a2a_signing_keys` — ES256 keypairs that sign outbound `X-A2A-Signature`
--     JWS headers. The private key is AES-256-GCM-encrypted with a KEK derived
--     from `auth.jwt_secret` (HKDF-SHA256). The public key is published as a
--     JWK at `/.well-known/jwks.json`. Two keys overlap during the rotation
--     window so subscribers caching by `kid` see both old and new keys.
--   * `a2a_push_dead_letter` — one row per push delivery that exhausted its
--     retry budget. Backs the dashboards in ADR-0022.

CREATE TABLE a2a_signing_keys (
    id                          UUID PRIMARY KEY,
    kid                         TEXT NOT NULL UNIQUE,
    alg                         TEXT NOT NULL,
    public_key_jwk              JSONB NOT NULL,
    private_key_pem_encrypted   BYTEA NOT NULL,
    private_key_nonce           BYTEA NOT NULL,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    activates_at                TIMESTAMPTZ NOT NULL,
    expires_at                  TIMESTAMPTZ NOT NULL,
    rotated_out_at              TIMESTAMPTZ
);

CREATE INDEX idx_a2a_signing_keys_active ON a2a_signing_keys (expires_at);

COMMENT ON COLUMN a2a_signing_keys.alg IS
  'JWS algorithm name (currently only ES256; EdDSA may join later under a separate kid).';
COMMENT ON COLUMN a2a_signing_keys.activates_at IS
  'Earliest moment this key may be used to sign a new payload.';
COMMENT ON COLUMN a2a_signing_keys.expires_at IS
  'Latest moment this key may appear in the JWKS response (includes overlap window).';
COMMENT ON COLUMN a2a_signing_keys.rotated_out_at IS
  'Set when a successor key is generated; signing flips to the successor immediately.';

CREATE TABLE a2a_push_dead_letter (
    id              UUID PRIMARY KEY,
    task_id         UUID NOT NULL,
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    config_id       UUID,
    url             TEXT NOT NULL,
    last_status     INT,
    last_error      TEXT,
    attempts        INT NOT NULL,
    payload         JSONB NOT NULL,
    failed_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_a2a_push_dead_letter_task   ON a2a_push_dead_letter (task_id);
CREATE INDEX idx_a2a_push_dead_letter_tenant ON a2a_push_dead_letter (tenant_id);

ALTER TABLE a2a_push_dead_letter ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation_a2a_push_dead_letter ON a2a_push_dead_letter
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);

COMMENT ON COLUMN a2a_push_dead_letter.config_id IS
  'Originating a2a_push_configs.id when known; nullable because the config may have been deleted by the janitor before the worker exhausted its retries.';
COMMENT ON COLUMN a2a_push_dead_letter.payload IS
  'Final payload that would have been delivered. Stored verbatim so the dashboard can replay manually.';
