//! Persistence port for the ES256 keypairs that sign outbound A2A push
//! notifications (ADR [`0009`](../../../docs/adrs/0009-push-notifications.md)
//! §`Signing — JWS over the payload`).
//!
//! Each row stores:
//!
//! - `kid` — the key id surfaced in `X-A2A-Key-Id` and the JWKS response.
//! - `public_key_jwk` — the JWK shape served at `/.well-known/jwks.json`.
//! - `private_key_pem_encrypted` + `private_key_nonce` — AES-256-GCM ciphertext
//!   sealed with a KEK derived from `auth.jwt_secret` (HKDF-SHA256). Plaintext
//!   never touches Postgres or the JWKS path.
//!
//! Tenant scoping does not apply: signing keys are mesh-wide. Subscribers verify
//! against the public JWK regardless of tenant.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct A2aSigningKeyRow {
    pub id: Uuid,
    pub kid: String,
    pub alg: String,
    pub public_key_jwk: serde_json::Value,
    pub private_key_pem_encrypted: Vec<u8>,
    pub private_key_nonce: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub activates_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub rotated_out_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait A2aSigningKeyRepository: Send + Sync {
    /// Insert a freshly-generated signing key. Implementations enforce the
    /// `kid` UNIQUE constraint at the DB layer; a duplicate `kid` is treated
    /// as `OrkError::Conflict`.
    async fn insert(&self, row: &A2aSigningKeyRow) -> Result<(), OrkError>;

    /// Return every key whose `expires_at > now` ordered by `created_at`
    /// descending. Used by the JWKS provider to publish all active keys
    /// (including those still in their 7-day overlap window) and by the signer
    /// to pick the most recent key.
    async fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<A2aSigningKeyRow>, OrkError>;

    /// Stamp `rotated_out_at = at` on the row identified by `id`. Called when a
    /// successor key has just been generated; the rotated-out key remains in the
    /// JWKS until `expires_at` so subscribers caching by `kid` still verify
    /// in-flight requests.
    async fn mark_rotated(&self, id: Uuid, at: DateTime<Utc>) -> Result<(), OrkError>;
}
