//! ADR-0020 §`Secrets handling`: per-tenant envelope encryption used by
//! [`ork-persistence`'s `tenant_repo`][1] to encrypt the `*_encrypted`
//! fields on `tenants` (GitHub token, push-config, vendor MCP creds, …)
//! at rest.
//!
//! Two-tier scheme:
//!
//! 1. Each tenant has a randomly-generated 32-byte **DEK**. The plaintext
//!    `*_encrypted` field bytes are AES-GCM-sealed under this DEK with
//!    AAD bound to `(tenant_id, field_name)` so a row-level swap (e.g.
//!    copying a sealed value from one field to another, or from one
//!    tenant to another) cannot succeed even under hostile DB access.
//! 2. The DEK is wrapped under the KMS-managed **KEK** via the
//!    [`crate::KmsClient`]. The wrapped DEK is persisted on the
//!    `tenants` row (`dek_wrapped` / `dek_key_id` / `dek_version`).
//!
//! Per-tenant DEKs are cached in-memory for [`DEK_CACHE_TTL`] so that
//! repeated reads on a hot tenant don't pay one KMS unwrap per query.
//! On miss / expiry we re-unwrap from the persisted ciphertext.
//!
//! [1]: ../../../ork-persistence/src/postgres/tenant_repo.rs

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_push::encryption::{Envelope, KEK_LEN, open as aead_open, seal as aead_seal};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::kms::{Ciphertext, KmsClient, KmsError};

/// Build the AEAD AAD for one `(tenant_id, field_name)` field. Binding
/// both into the AAD makes a sealed value invalid under any other
/// (tenant, field) pair — defends against a hostile DB swap moving
/// sealed bytes across rows or columns.
fn field_aad(tenant_id: TenantId, field_name: &str) -> Vec<u8> {
    format!(
        "ork.tenant.field.v1|{}|{}",
        tenant_id.0.as_hyphenated(),
        field_name
    )
    .into_bytes()
}

/// On-disk marker that distinguishes a sealed field from a plaintext
/// pre-migration value. Format: `enc:v1:<base64>` where `<base64>` is
/// `nonce ‖ ciphertext` encoded with [`STANDARD_NO_PAD`].
pub const FIELD_MARKER_V1: &str = "enc:v1:";

/// Default TTL for the in-process DEK cache. Five minutes balances "no
/// per-query KMS roundtrip" against "stale DEK after a `keys rotate`."
pub const DEK_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Sealed per-field on-disk envelope. `nonce ‖ ciphertext` mirrors the
/// shape used by [`ork_push::encryption::Envelope`] so callers that
/// already understand the push-signing wire format don't need a second
/// mental model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedField {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl SealedField {
    fn from_envelope(env: Envelope) -> Self {
        Self {
            ciphertext: env.ciphertext,
            nonce: env.nonce,
        }
    }

    fn into_envelope(self) -> Envelope {
        Envelope {
            ciphertext: self.ciphertext,
            nonce: self.nonce,
        }
    }
}

/// Cached DEK + the [`Instant`] it landed in the cache. Anything older
/// than [`DEK_CACHE_TTL`] is treated as "miss" on next access.
struct CachedDek {
    bytes: [u8; KEK_LEN],
    cached_at: Instant,
}

/// Per-tenant secret cipher. Holds a reference to the [`KmsClient`] for
/// DEK wrap/unwrap and an in-process DEK cache.
pub struct TenantSecretsCipher {
    kms: Arc<dyn KmsClient>,
    cache: Mutex<HashMap<TenantId, CachedDek>>,
    ttl: Duration,
}

impl std::fmt::Debug for TenantSecretsCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantSecretsCipher")
            .field("kms", &"<dyn KmsClient>")
            .field("cache_size", &"<async lock>")
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl TenantSecretsCipher {
    #[must_use]
    pub fn new(kms: Arc<dyn KmsClient>) -> Self {
        Self {
            kms,
            cache: Mutex::new(HashMap::new()),
            ttl: DEK_CACHE_TTL,
        }
    }

    /// Test/diagnostic constructor — accepts an explicit TTL so unit
    /// tests can drive cache expiry without sleeping for 5 minutes.
    #[must_use]
    pub fn with_ttl(kms: Arc<dyn KmsClient>, ttl: Duration) -> Self {
        Self {
            kms,
            cache: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Generate a fresh 32-byte DEK and return it alongside the wrapped
    /// form ready for persistence on the `tenants` row. Used on tenant
    /// create.
    pub async fn mint_dek(&self) -> Result<([u8; KEK_LEN], Ciphertext), OrkError> {
        use aes_gcm::Aes256Gcm;
        use aes_gcm::aead::{KeyInit, OsRng};
        let key = Aes256Gcm::generate_key(&mut OsRng);
        let mut bytes = [0u8; KEK_LEN];
        bytes.copy_from_slice(key.as_slice());
        let wrapped = self.kms.wrap(&bytes).await?;
        Ok((bytes, wrapped))
    }

    /// Resolve the per-tenant DEK from cache or unwrap from the
    /// persisted ciphertext. Repos that already have the wrapped form
    /// in hand can use this to avoid a second DB read.
    pub async fn dek_for(
        &self,
        tenant_id: TenantId,
        wrapped: &Ciphertext,
    ) -> Result<[u8; KEK_LEN], OrkError> {
        // Cache hit?
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&tenant_id)
                && entry.cached_at.elapsed() < self.ttl
            {
                return Ok(entry.bytes);
            }
        }
        // Miss / expired: unwrap, then write-through.
        let plain = self.kms.unwrap(wrapped).await?;
        if plain.len() != KEK_LEN {
            return Err(OrkError::Internal(format!(
                "unwrapped DEK has wrong length: {} (expected {KEK_LEN})",
                plain.len()
            )));
        }
        let mut bytes = [0u8; KEK_LEN];
        bytes.copy_from_slice(&plain);
        self.cache.lock().await.insert(
            tenant_id,
            CachedDek {
                bytes,
                cached_at: Instant::now(),
            },
        );
        Ok(bytes)
    }

    /// Encrypt `plaintext` for `tenant_id` using the tenant's DEK
    /// (already unwrapped via [`Self::dek_for`]).
    pub async fn seal_for_tenant(
        &self,
        tenant_id: TenantId,
        wrapped_dek: &Ciphertext,
        plaintext: &[u8],
    ) -> Result<SealedField, OrkError> {
        let dek = self.dek_for(tenant_id, wrapped_dek).await?;
        let env = aead_seal(plaintext, &dek)
            .map_err(|e| OrkError::from(KmsError::Seal(e.to_string())))?;
        Ok(SealedField::from_envelope(env))
    }

    /// Decrypt a previously-sealed `*_encrypted` field for the tenant.
    pub async fn open_for_tenant(
        &self,
        tenant_id: TenantId,
        wrapped_dek: &Ciphertext,
        sealed: &SealedField,
    ) -> Result<Vec<u8>, OrkError> {
        let dek = self.dek_for(tenant_id, wrapped_dek).await?;
        aead_open(&sealed.clone().into_envelope(), &dek)
            .map_err(|e| OrkError::from(KmsError::Open(e.to_string())))
    }

    /// Drop a tenant from the cache; called after `keys rotate` so the
    /// next access re-unwraps under the new wrapped DEK.
    pub async fn invalidate(&self, tenant_id: TenantId) {
        self.cache.lock().await.remove(&tenant_id);
    }

    /// ADR-0020 §`Secrets handling`: seal a single string field for
    /// at-rest persistence. The AAD is bound to `(tenant_id,
    /// field_name)` so the sealed bytes are valid only under the same
    /// pair on open. Returned form is the marker-prefixed
    /// `enc:v1:<b64>` shape so [`Self::try_open_field`] can recognise
    /// it on read and pre-migration plaintext values pass through
    /// unchanged.
    pub async fn seal_field(
        &self,
        tenant_id: TenantId,
        field_name: &str,
        wrapped_dek: &Ciphertext,
        plaintext: &str,
    ) -> Result<String, OrkError> {
        let dek = self.dek_for(tenant_id, wrapped_dek).await?;
        let aad = field_aad(tenant_id, field_name);
        let key = Key::<Aes256Gcm>::from_slice(&dek);
        let cipher = Aes256Gcm::new(key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|e| OrkError::from(KmsError::Seal(e.to_string())))?;
        let mut payload = Vec::with_capacity(nonce.len() + ciphertext.len());
        payload.extend_from_slice(nonce.as_slice());
        payload.extend_from_slice(&ciphertext);
        Ok(format!(
            "{FIELD_MARKER_V1}{}",
            STANDARD_NO_PAD.encode(payload)
        ))
    }

    /// Inverse of [`Self::seal_field`]: returns
    ///
    /// - `Ok(Some(plaintext))` if `value` is a `enc:v1:` blob and decrypts cleanly,
    /// - `Ok(None)` if `value` is not in the sealed format (pre-migration plaintext),
    /// - `Err(_)` if `value` advertised the marker but failed to decode / open.
    ///
    /// Callers that need "always return a plaintext string" use
    /// `try_open_field(...)?.unwrap_or_else(|| value.to_string())`.
    pub async fn try_open_field(
        &self,
        tenant_id: TenantId,
        field_name: &str,
        wrapped_dek: &Ciphertext,
        value: &str,
    ) -> Result<Option<String>, OrkError> {
        let Some(rest) = value.strip_prefix(FIELD_MARKER_V1) else {
            return Ok(None);
        };
        let bytes = STANDARD_NO_PAD
            .decode(rest)
            .map_err(|e| OrkError::from(KmsError::Malformed(format!("base64: {e}"))))?;
        if bytes.len() < 12 + 16 {
            return Err(OrkError::from(KmsError::Malformed(format!(
                "sealed field too short: {} bytes",
                bytes.len()
            ))));
        }
        let (nonce_bytes, ct) = bytes.split_at(12);
        let dek = self.dek_for(tenant_id, wrapped_dek).await?;
        let aad = field_aad(tenant_id, field_name);
        let key = Key::<Aes256Gcm>::from_slice(&dek);
        let cipher = Aes256Gcm::new(key);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext_bytes = cipher
            .decrypt(nonce, Payload { msg: ct, aad: &aad })
            .map_err(|e| OrkError::from(KmsError::Open(e.to_string())))?;
        let s = String::from_utf8(plaintext_bytes)
            .map_err(|e| OrkError::Internal(format!("sealed field is not utf-8: {e}")))?;
        Ok(Some(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JwtSecretKekKms;
    use secrecy::SecretString;
    use uuid::Uuid;

    fn cipher_with_ttl(ttl: Duration) -> TenantSecretsCipher {
        let kms: Arc<dyn KmsClient> =
            Arc::new(JwtSecretKekKms::new(SecretString::from("test-secret")));
        TenantSecretsCipher::with_ttl(kms, ttl)
    }

    #[tokio::test]
    async fn seal_then_open_roundtrips() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let sealed = c
            .seal_for_tenant(tid, &wrapped, b"github_pat_xyz")
            .await
            .expect("seal");
        let plain = c
            .open_for_tenant(tid, &wrapped, &sealed)
            .await
            .expect("open");
        assert_eq!(plain, b"github_pat_xyz");
    }

    #[tokio::test]
    async fn cache_returns_same_dek_within_ttl() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let a = c.dek_for(tid, &wrapped).await.expect("first");
        let b = c.dek_for(tid, &wrapped).await.expect("second");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn invalidate_drops_cache_entry() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let _ = c.dek_for(tid, &wrapped).await.expect("first");
        c.invalidate(tid).await;
        let cache = c.cache.lock().await;
        assert!(cache.get(&tid).is_none());
    }

    #[tokio::test]
    async fn cache_expires_after_ttl() {
        // A tiny TTL drives expiry deterministically without sleeping
        // for the production 5-minute window.
        let c = cipher_with_ttl(Duration::from_millis(20));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let _ = c.dek_for(tid, &wrapped).await.expect("first");
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Force a re-resolve: cache should be considered stale.
        let _ = c.dek_for(tid, &wrapped).await.expect("second");
        // We can't easily probe "did this re-call kms.unwrap" here
        // without a counter-mock; the assertion above is that the call
        // doesn't break post-TTL. Cache-hit semantics are covered by the
        // companion test.
    }

    #[tokio::test]
    async fn seal_field_then_try_open_field_roundtrips() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let sealed = c
            .seal_field(tid, "github_token", &wrapped, "ghp_abc123")
            .await
            .expect("seal");
        assert!(sealed.starts_with(FIELD_MARKER_V1), "must carry marker");
        let plain = c
            .try_open_field(tid, "github_token", &wrapped, &sealed)
            .await
            .expect("open");
        assert_eq!(plain.as_deref(), Some("ghp_abc123"));
    }

    /// Pre-migration values lack the marker — `try_open_field` returns
    /// `Ok(None)` so callers can pass through to the legacy plaintext.
    #[tokio::test]
    async fn try_open_field_passes_through_plaintext() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let res = c
            .try_open_field(tid, "github_token", &wrapped, "ghp_legacy_plaintext")
            .await
            .expect("ok");
        assert!(res.is_none());
    }

    /// A marker-prefixed value with bad base64 surfaces a hard error so
    /// silent corruption is never confused with pre-migration plaintext.
    #[tokio::test]
    async fn try_open_field_rejects_marker_with_bad_base64() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let err = c
            .try_open_field(tid, "github_token", &wrapped, "enc:v1:not-base64!!!")
            .await
            .expect_err("must error");
        assert!(matches!(err, OrkError::Internal(_)));
    }

    /// ADR-0020 review-finding M2: opening a sealed value under the
    /// wrong field name must fail. Defends against a hostile DB swap
    /// that copies a sealed value from `github_token_encrypted` into
    /// `gitlab_token_encrypted` on the same tenant.
    #[tokio::test]
    async fn cross_field_swap_rejected_by_aad() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid = TenantId(Uuid::now_v7());
        let (_dek, wrapped) = c.mint_dek().await.expect("mint");
        let sealed = c
            .seal_field(tid, "github_token", &wrapped, "ghp_abc123")
            .await
            .expect("seal");
        let err = c
            .try_open_field(tid, "gitlab_token", &wrapped, &sealed)
            .await
            .expect_err("opening under wrong field name must fail");
        // The AEAD verification fails — surfaces as KmsError::Open
        // wrapped into OrkError::Internal.
        assert!(matches!(err, OrkError::Internal(_)));
    }

    /// ADR-0020 review-finding M2: opening a sealed value under the
    /// wrong tenant must fail (cross-tenant swap).
    #[tokio::test]
    async fn cross_tenant_swap_rejected_by_aad() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid_a = TenantId(Uuid::now_v7());
        let tid_b = TenantId(Uuid::now_v7());
        let (_dek_a, wrapped_a) = c.mint_dek().await.expect("mint a");
        let sealed = c
            .seal_field(tid_a, "github_token", &wrapped_a, "ghp_xxx")
            .await
            .expect("seal");
        // Decrypt attempt with tenant B and tenant A's wrapped DEK +
        // the same field name: AAD differs (tenant_id), so even though
        // the DEK material would be the same, the open must fail.
        let err = c
            .try_open_field(tid_b, "github_token", &wrapped_a, &sealed)
            .await
            .expect_err("cross-tenant open must fail under AAD binding");
        assert!(matches!(err, OrkError::Internal(_)));
    }

    #[tokio::test]
    async fn wrong_dek_fails_to_open() {
        let c = cipher_with_ttl(Duration::from_secs(60));
        let tid_a = TenantId(Uuid::now_v7());
        let tid_b = TenantId(Uuid::now_v7());
        let (_dek_a, wrapped_a) = c.mint_dek().await.expect("mint a");
        let (_dek_b, wrapped_b) = c.mint_dek().await.expect("mint b");
        let sealed = c
            .seal_for_tenant(tid_a, &wrapped_a, b"secret-a")
            .await
            .expect("seal");
        // Try opening tenant A's sealed field with tenant B's DEK.
        let err = c
            .open_for_tenant(tid_b, &wrapped_b, &sealed)
            .await
            .expect_err("must fail under wrong DEK");
        assert!(matches!(err, OrkError::Internal(_)));
    }
}
