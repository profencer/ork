//! ADR-0020 §`Secrets handling`: the [`KmsClient`] port and the legacy
//! `JwtSecretKekKms` adapter that preserves today's KEK-derived-from-JWT
//! behaviour without forcing operators to wire a real KMS provider.
//!
//! Trait shape matches the per-vendor KMS APIs (AWS KMS `Encrypt` /
//! `Decrypt`, GCP KMS, Vault Transit, Azure Key Vault) so the four cloud
//! adapters in the ADR roadmap can drop in behind the same trait. For
//! Phase C1 only the legacy adapter ships; the cloud adapters
//! (Phase C2-C5) are deferred to follow-up ADRs per the user decision
//! recorded in `docs/adrs/0020-tenant-security-and-trust.md`
//! §`Reviewer findings`.
//!
//! Two pieces:
//!
//! - [`KmsClient`] — wrap / unwrap arbitrary bytes (typically a per-tenant
//!   DEK; see [`crate::tenant_cipher::TenantSecretsCipher`]) plus an
//!   explicit [`KmsClient::derive_kek_compat`] back-door for the
//!   ork-push signing-key path that already encrypts directly under a
//!   KEK and must keep its on-disk format stable across this migration.
//! - [`JwtSecretKekKms`] — the legacy adapter. KEK is HKDF-derived from
//!   `auth.jwt_secret` (matching `ork_push::encryption::derive_kek` byte
//!   for byte) so existing push-signing rows decrypt unchanged when
//!   `[security.kms].provider = "legacy"`.
//!
//! Forward-looking note: [`Ciphertext`] carries `key_id` and `version`
//! columns that `JwtSecretKekKms` always sets to `None` / `Some("v1")`
//! respectively. Cloud adapters populate `key_id` (e.g. KMS ARN) so a
//! later "rotate KEK" admin command can re-wrap DEKs whose key_id no
//! longer matches the configured one.

use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use async_trait::async_trait;
use hkdf::Hkdf;
use ork_common::error::OrkError;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

/// HKDF salt for the *tenant DEK wrapper* (distinct from
/// `ork_push::encryption::KEK_SALT`). Kept here so `JwtSecretKekKms`
/// doesn't bind a tenant DEK under the same KEK material the push
/// signing path uses — defense in depth in case one HKDF context is
/// later extracted but not the other.
pub const TENANT_DEK_SALT: &[u8] = b"ork.security.tenant.dek.kek.v1";
pub const TENANT_DEK_INFO: &[u8] = b"ork-security/tenant-dek-wrapper";
pub const KEK_LEN: usize = 32;

/// Errors specific to KMS operations. Wrapped into [`OrkError::Internal`]
/// when surfaced to repository / handler code.
#[derive(Debug, Error)]
pub enum KmsError {
    #[error("HKDF expand failed: {0}")]
    Hkdf(String),
    #[error("AEAD seal failed: {0}")]
    Seal(String),
    #[error("AEAD open failed (key/ciphertext/nonce mismatch or tampered): {0}")]
    Open(String),
    #[error("malformed wrapped DEK: {0}")]
    Malformed(String),
    #[error("KMS provider error: {0}")]
    Provider(String),
}

impl From<KmsError> for OrkError {
    fn from(err: KmsError) -> Self {
        OrkError::Internal(format!("kms: {err}"))
    }
}

/// On-wire wrapped form of a DEK. The byte layout (or KMS-side reference)
/// is opaque to callers; `key_id` and `version` are observability /
/// rotation metadata, persisted alongside the wrapped bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ciphertext {
    /// The wrapped DEK as opaque bytes. For [`JwtSecretKekKms`] this is a
    /// nonce ‖ ciphertext concatenation (12 bytes nonce, then AES-GCM
    /// ciphertext). For cloud adapters this is whatever the provider's
    /// `Encrypt` API returns.
    pub bytes: Vec<u8>,
    /// Provider-side key identifier (e.g. AWS KMS ARN, GCP KMS resource
    /// name). `None` for the legacy adapter where the KEK is implicit.
    pub key_id: Option<String>,
    /// On-wire version tag. `Some("v1")` for everything that ships in
    /// this PR; bump on any breaking change to the byte layout.
    pub version: Option<String>,
}

impl Ciphertext {
    pub const CURRENT_VERSION: &'static str = "v1";
}

/// Port for envelope encryption: wrap / unwrap a per-tenant DEK against
/// a KMS-managed KEK. The trait is small on purpose — every cloud
/// provider exposes a richer surface (Encrypt / Decrypt / Sign / etc.)
/// but ork only needs DEK wrapping today.
#[async_trait]
pub trait KmsClient: Send + Sync {
    /// Wrap `plaintext` (typically a 32-byte DEK) under the KMS-managed
    /// KEK. The resulting [`Ciphertext`] is what gets persisted on
    /// `tenants.dek_wrapped`.
    async fn wrap(&self, plaintext: &[u8]) -> Result<Ciphertext, KmsError>;

    /// Unwrap a previously-wrapped DEK back to its plaintext bytes.
    /// Authentication (tag verification, signature, etc.) is done by
    /// the underlying provider — a tampered ciphertext never returns Ok.
    async fn unwrap(&self, ciphertext: &Ciphertext) -> Result<Vec<u8>, KmsError>;

    /// Provider-side rotation: when the KMS supports key versions /
    /// rotation (AWS KMS `key-rotation`, GCP KMS `cryptoKeyVersions`),
    /// roll the active version. No-op on the legacy adapter — there
    /// the KEK is derived from `auth.jwt_secret` and rotation means the
    /// operator changes that secret out-of-band.
    async fn rotate(&self) -> Result<(), KmsError>;

    /// ADR-0009 push-signing keys are stored under a KEK-direct AES-GCM
    /// envelope (`Envelope { ciphertext, nonce }` — see
    /// [`ork_push::encryption`]). Until ork-push adopts the
    /// [`KmsClient::wrap`] / `unwrap` shape, it needs to read the raw
    /// 32-byte KEK to keep its existing on-disk format. Cloud adapters
    /// either implement this against a KMS data-key (e.g. AWS KMS
    /// `GenerateDataKey`) or hard-error if the provider has no
    /// equivalent — in which case operators must run the push-signing
    /// migration before flipping `[security.kms].provider`.
    async fn derive_kek_compat(&self) -> Result<[u8; KEK_LEN], KmsError>;
}

/// Legacy adapter: KEK is HKDF-derived from `auth.jwt_secret` using the
/// same `(salt, info)` pair as [`ork_push::encryption::derive_kek`]. So
/// for `[security.kms].provider = "legacy"` (the default) the
/// [`KmsClient::derive_kek_compat`] path returns the exact same KEK
/// ork-push has been deriving locally — no rewrap needed.
///
/// Tenant-side `wrap`/`unwrap` use a *separate* HKDF context
/// ([`TENANT_DEK_SALT`] / [`TENANT_DEK_INFO`]) so the push-signing KEK
/// and the tenant-DEK-wrapper KEK are cryptographically distinct even
/// though both are derived from the same secret.
pub struct JwtSecretKekKms {
    secret: SecretString,
}

impl std::fmt::Debug for JwtSecretKekKms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtSecretKekKms")
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl JwtSecretKekKms {
    #[must_use]
    pub fn new(secret: SecretString) -> Self {
        Self { secret }
    }

    /// Convenience: wrap into an `Arc<dyn KmsClient>` so AppState wiring
    /// is a one-liner.
    #[must_use]
    pub fn arc(self) -> Arc<dyn KmsClient> {
        Arc::new(self)
    }

    fn wrap_kek(&self) -> [u8; KEK_LEN] {
        let hk = Hkdf::<Sha256>::new(
            Some(TENANT_DEK_SALT),
            self.secret.expose_secret().as_bytes(),
        );
        let mut out = [0u8; KEK_LEN];
        hk.expand(TENANT_DEK_INFO, &mut out)
            .expect("HKDF expand of 32 bytes from SHA-256 cannot fail");
        out
    }
}

#[async_trait]
impl KmsClient for JwtSecretKekKms {
    async fn wrap(&self, plaintext: &[u8]) -> Result<Ciphertext, KmsError> {
        let kek = self.wrap_kek();
        let key = Key::<Aes256Gcm>::from_slice(&kek);
        let cipher = Aes256Gcm::new(key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: TENANT_DEK_INFO,
                },
            )
            .map_err(|e| KmsError::Seal(e.to_string()))?;
        // Layout: 12 bytes nonce ‖ ciphertext.
        let mut bytes = Vec::with_capacity(12 + ct.len());
        bytes.extend_from_slice(nonce.as_slice());
        bytes.extend_from_slice(&ct);
        Ok(Ciphertext {
            bytes,
            key_id: None,
            version: Some(Ciphertext::CURRENT_VERSION.to_string()),
        })
    }

    async fn unwrap(&self, ciphertext: &Ciphertext) -> Result<Vec<u8>, KmsError> {
        if ciphertext.bytes.len() < 12 + 16 {
            return Err(KmsError::Malformed(format!(
                "wrapped DEK too short: {} bytes (need >= 28)",
                ciphertext.bytes.len()
            )));
        }
        let (nonce_bytes, ct) = ciphertext.bytes.split_at(12);
        let kek = self.wrap_kek();
        let key = Key::<Aes256Gcm>::from_slice(&kek);
        let cipher = Aes256Gcm::new(key);
        let nonce = Nonce::from_slice(nonce_bytes);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ct,
                    aad: TENANT_DEK_INFO,
                },
            )
            .map_err(|e| KmsError::Open(e.to_string()))
    }

    async fn rotate(&self) -> Result<(), KmsError> {
        // Legacy KEK rotation = operator changes `auth.jwt_secret` and
        // re-runs `ork admin keys rotate --scope tenants` to re-wrap
        // every tenant DEK under the new KEK. No KMS-side action.
        Ok(())
    }

    async fn derive_kek_compat(&self) -> Result<[u8; KEK_LEN], KmsError> {
        Ok(ork_push::encryption::derive_kek(
            self.secret.expose_secret(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kms() -> JwtSecretKekKms {
        JwtSecretKekKms::new(SecretString::from("test-jwt-secret"))
    }

    #[tokio::test]
    async fn wrap_then_unwrap_roundtrips() {
        let k = kms();
        let dek = b"some-32-byte-dek-here-padding-ok";
        let ct = k.wrap(dek).await.expect("wrap");
        let back = k.unwrap(&ct).await.expect("unwrap");
        assert_eq!(back, dek.to_vec());
        assert_eq!(ct.version.as_deref(), Some("v1"));
        assert!(ct.key_id.is_none());
    }

    #[tokio::test]
    async fn wrong_secret_fails_to_unwrap() {
        let a = JwtSecretKekKms::new(SecretString::from("secret-a"));
        let b = JwtSecretKekKms::new(SecretString::from("secret-b"));
        let ct = a.wrap(b"x").await.expect("wrap");
        b.unwrap(&ct)
            .await
            .expect_err("wrong KEK must fail to unwrap");
    }

    #[tokio::test]
    async fn malformed_ciphertext_is_validation_error() {
        let k = kms();
        let bad = Ciphertext {
            bytes: vec![0u8; 4], // way too short
            key_id: None,
            version: Some("v1".into()),
        };
        let err = k.unwrap(&bad).await.expect_err("must reject");
        assert!(matches!(err, KmsError::Malformed(_)));
    }

    #[tokio::test]
    async fn derive_kek_compat_matches_ork_push() {
        let secret = "shared-jwt";
        let k = JwtSecretKekKms::new(SecretString::from(secret));
        let got = k.derive_kek_compat().await.expect("derive");
        let expected = ork_push::encryption::derive_kek(secret);
        assert_eq!(got, expected, "compat KEK must match ork-push exactly");
    }

    #[tokio::test]
    async fn tenant_kek_differs_from_push_kek() {
        // Same secret, different HKDF context → different KEKs.
        let secret = "shared-jwt";
        let k = JwtSecretKekKms::new(SecretString::from(secret));
        let push_kek = ork_push::encryption::derive_kek(secret);
        // Wrap a known DEK then attempt to unwrap with the push KEK
        // pretending it's the tenant KEK — must fail (different keys).
        let ct = k.wrap(b"d").await.expect("wrap");
        // Manually attempt decrypt under push_kek to confirm domain
        // separation. (We don't expose a public unwrap-with-kek helper
        // — this is a defensive assertion.)
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes256Gcm, Key, Nonce};
        let (nonce_bytes, body) = ct.bytes.split_at(12);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&push_kek));
        let nonce = Nonce::from_slice(nonce_bytes);
        let attempt = cipher.decrypt(
            nonce,
            Payload {
                msg: body,
                aad: TENANT_DEK_INFO,
            },
        );
        assert!(
            attempt.is_err(),
            "push KEK must NOT decrypt a tenant-DEK-wrapped ciphertext"
        );
    }

    #[test]
    fn debug_redacts_secret() {
        let dbg = format!("{:?}", kms());
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("test-jwt-secret"));
    }
}
