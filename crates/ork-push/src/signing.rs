//! ES256 signing material, JWKS provider, and rotation logic for ADR-0009.
//!
//! Each [`SigningKeyMaterial`] is loaded from `a2a_signing_keys`, decrypted with
//! the [`crate::encryption`] envelope helper, and held in memory as a
//! [`p256::SecretKey`] alongside the cached PEM that `jsonwebtoken` consumes.
//!
//! Two operations matter at runtime:
//!
//! * [`JwksProvider::sign_detached`] — every outbound push notification body is
//!   hashed with SHA-256 and signed under the most-recent active key. The
//!   compact detached JWS (`<header>..<signature>`) goes in the
//!   `X-A2A-Signature` header; the `kid` mirrors that header in `X-A2A-Key-Id`.
//! * [`JwksProvider::jwks`] — the `/.well-known/jwks.json` payload. During the
//!   rotation overlap window both the new and the rotated-out keys appear so
//!   subscribers caching by `kid` keep verifying in-flight requests.
//!
//! [`RotationPolicy`] captures the two knobs surfaced through `config.push`:
//! `key_rotation_days` (when a successor is generated) and `key_overlap_days`
//! (how long the old key stays in JWKS once a successor exists).

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey};
use ork_common::error::OrkError;
use ork_core::ports::a2a_signing_key_repo::{A2aSigningKeyRepository, A2aSigningKeyRow};
use p256::SecretKey;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::encryption::{self, Envelope, KEK_LEN};

/// Per-call material the signer needs.
#[derive(Clone)]
pub struct SigningKeyMaterial {
    pub id: Uuid,
    pub kid: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub rotated_out_at: Option<DateTime<Utc>>,
    /// Public JWK as stored in `a2a_signing_keys.public_key_jwk` plus the
    /// `kid`/`alg`/`use` envelope fields the JWKS endpoint serves verbatim.
    pub public_key_jwk: Value,
    /// PKCS#8 PEM that `jsonwebtoken::EncodingKey::from_ec_pem` consumes. Held
    /// in `Arc` so cloning the material is cheap and the secret bytes live in
    /// one place.
    pem: Arc<Zeroizing<String>>,
}

impl SigningKeyMaterial {
    fn encoding_key(&self) -> Result<EncodingKey, SigningError> {
        EncodingKey::from_ec_pem(self.pem.as_bytes())
            .map_err(|e| SigningError::Internal(format!("load ES256 PEM: {e}")))
    }
}

/// Rotation knobs (mirrors `config.push.key_rotation_days` /
/// `key_overlap_days`).
#[derive(Clone, Copy, Debug)]
pub struct RotationPolicy {
    pub rotation_days: u32,
    pub overlap_days: u32,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            rotation_days: 30,
            overlap_days: 7,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotateOutcome {
    pub new_kid: String,
    pub new_expires_at: DateTime<Utc>,
    /// Pre-existing key whose `rotated_out_at` was just stamped, if any.
    pub previous_kid: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("no active signing key — boot path failed to ensure one")]
    NoActiveKey,
    #[error("storage error: {0}")]
    Storage(#[from] OrkError),
    #[error("envelope encryption error: {0}")]
    Envelope(#[from] encryption::EncryptionError),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Default, Clone)]
struct Snapshot {
    keys: Vec<SigningKeyMaterial>,
    jwks: Value,
}

/// In-memory cache of the currently-published signing keys. `refresh` repopulates
/// the snapshot from the repository (decrypting private material via the KEK);
/// callers can opt in to a lightweight 60s background refresh, but rotations
/// trigger an immediate refresh on the same task.
pub struct JwksProvider {
    repo: Arc<dyn A2aSigningKeyRepository>,
    kek: [u8; KEK_LEN],
    policy: RotationPolicy,
    snapshot: RwLock<Snapshot>,
}

impl JwksProvider {
    /// Build a provider and populate the initial snapshot.
    pub async fn new(
        repo: Arc<dyn A2aSigningKeyRepository>,
        kek: [u8; KEK_LEN],
        policy: RotationPolicy,
    ) -> Result<Arc<Self>, SigningError> {
        let provider = Arc::new(Self {
            repo,
            kek,
            policy,
            snapshot: RwLock::new(Snapshot::default()),
        });
        provider.refresh().await?;
        Ok(provider)
    }

    /// Reload all active keys from the repository, decrypt their private
    /// material, and rebuild the cached `jwks` document.
    pub async fn refresh(&self) -> Result<(), SigningError> {
        let now = Utc::now();
        let rows = self.repo.list_active(now).await?;
        let mut keys = Vec::with_capacity(rows.len());
        for row in rows {
            keys.push(decrypt_row(&row, &self.kek)?);
        }
        let jwks = build_jwks(&keys);
        *self.snapshot.write().await = Snapshot { keys, jwks };
        Ok(())
    }

    /// JWKS payload served verbatim at `/.well-known/jwks.json`.
    pub async fn jwks(&self) -> Value {
        self.snapshot.read().await.jwks.clone()
    }

    /// The most-recent non-expired key, used to sign new payloads.
    pub async fn current_signer(&self) -> Option<SigningKeyMaterial> {
        let snap = self.snapshot.read().await;
        snap.keys
            .iter()
            .filter(|k| k.rotated_out_at.is_none())
            .max_by_key(|k| k.created_at)
            .or_else(|| snap.keys.iter().max_by_key(|k| k.created_at))
            .cloned()
    }

    /// Sign `body` with the current key and return `(detached_jws, kid)`.
    ///
    /// The detached JWS shape is `<protected_header>..<signature>` per
    /// RFC 7515 §A.5. The receiver re-attaches the payload by hashing the body
    /// with SHA-256 and base64url-encoding it.
    pub async fn sign_detached(&self, body: &[u8]) -> Result<(String, String), SigningError> {
        let key = self
            .current_signer()
            .await
            .ok_or(SigningError::NoActiveKey)?;
        let encoding = key.encoding_key()?;

        let header = json!({
            "alg": "ES256",
            "kid": key.kid,
            "typ": "JOSE+JSON",
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let payload_b64 = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
        let signing_input = format!("{header_b64}.{payload_b64}");
        let signature =
            jsonwebtoken::crypto::sign(signing_input.as_bytes(), &encoding, Algorithm::ES256)
                .map_err(|e| SigningError::Internal(format!("ES256 sign: {e}")))?;
        Ok((format!("{header_b64}..{signature}"), key.kid))
    }

    /// Generate a new keypair if `force` is true OR if every existing key was
    /// created more than `policy.rotation_days` ago. The previous current
    /// signer is stamped with `rotated_out_at = now` so callers see it
    /// immediately migrate signing to the new key while the old one stays in
    /// JWKS until its `expires_at`.
    pub async fn rotate_if_due(
        &self,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<Option<RotateOutcome>, SigningError> {
        let snap = self.snapshot.read().await.clone();
        let current = snap
            .keys
            .iter()
            .filter(|k| k.rotated_out_at.is_none())
            .max_by_key(|k| k.created_at)
            .cloned();

        if !force {
            if let Some(ref c) = current {
                let age = now - c.created_at;
                if age < Duration::days(i64::from(self.policy.rotation_days)) {
                    return Ok(None);
                }
            }
        }

        let row = generate_key_row(now, self.policy, &self.kek)?;
        self.repo.insert(&row).await?;
        if let Some(ref prev) = current {
            self.repo.mark_rotated(prev.id, now).await?;
        }
        self.refresh().await?;
        Ok(Some(RotateOutcome {
            new_kid: row.kid,
            new_expires_at: row.expires_at,
            previous_kid: current.map(|c| c.kid),
        }))
    }

    /// Boot path: generate the first key when the table is empty so the
    /// service can start serving JWKS / signed pushes from request 1.
    pub async fn ensure_at_least_one(&self, now: DateTime<Utc>) -> Result<(), SigningError> {
        let any = !self.snapshot.read().await.keys.is_empty();
        if any {
            return Ok(());
        }
        let row = generate_key_row(now, self.policy, &self.kek)?;
        self.repo.insert(&row).await?;
        self.refresh().await?;
        Ok(())
    }
}

/// Decrypt an `A2aSigningKeyRow` into in-memory [`SigningKeyMaterial`]. The
/// resulting JWK preserves the stored `public_key_jwk` and adds the
/// `kid`/`alg`/`use` envelope expected by JWKS subscribers.
fn decrypt_row(
    row: &A2aSigningKeyRow,
    kek: &[u8; KEK_LEN],
) -> Result<SigningKeyMaterial, SigningError> {
    let env = Envelope {
        ciphertext: row.private_key_pem_encrypted.clone(),
        nonce: row.private_key_nonce.clone(),
    };
    let pem_bytes = encryption::open(&env, kek)?;
    let pem = String::from_utf8(pem_bytes)
        .map_err(|e| SigningError::Internal(format!("non-utf8 PEM: {e}")))?;

    let mut jwk = match row.public_key_jwk.clone() {
        Value::Object(map) => Value::Object(map),
        other => {
            return Err(SigningError::Internal(format!(
                "expected JWK object, got {}",
                other
            )));
        }
    };
    if let Value::Object(ref mut map) = jwk {
        map.insert("kid".into(), Value::String(row.kid.clone()));
        map.insert("alg".into(), Value::String(row.alg.clone()));
        map.insert("use".into(), Value::String("sig".into()));
    }

    Ok(SigningKeyMaterial {
        id: row.id,
        kid: row.kid.clone(),
        created_at: row.created_at,
        expires_at: row.expires_at,
        rotated_out_at: row.rotated_out_at,
        public_key_jwk: jwk,
        pem: Arc::new(Zeroizing::new(pem)),
    })
}

fn build_jwks(keys: &[SigningKeyMaterial]) -> Value {
    json!({
        "keys": keys.iter().map(|k| k.public_key_jwk.clone()).collect::<Vec<_>>(),
    })
}

/// Generate a fresh ES256 keypair, encrypt the PEM with the KEK, and return a
/// row ready for `A2aSigningKeyRepository::insert`. `expires_at` covers the
/// full `rotation + overlap` window so subscribers caching by `kid` see the
/// key for as long as ADR-0009 promises.
fn generate_key_row(
    now: DateTime<Utc>,
    policy: RotationPolicy,
    kek: &[u8; KEK_LEN],
) -> Result<A2aSigningKeyRow, SigningError> {
    let secret = SecretKey::random(&mut OsRng);
    let public = secret.public_key();
    let pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| SigningError::Internal(format!("PKCS#8 PEM encode: {e}")))?;

    let envelope = encryption::seal(pem.as_bytes(), kek)?;

    let jwk_value: Value = serde_json::from_str(&public.to_jwk_string())
        .map_err(|e| SigningError::Internal(format!("JWK serialize: {e}")))?;

    let kid = format!("k_{}", Uuid::now_v7().simple());
    let total = i64::from(policy.rotation_days) + i64::from(policy.overlap_days);

    Ok(A2aSigningKeyRow {
        id: Uuid::now_v7(),
        kid,
        alg: "ES256".into(),
        public_key_jwk: jwk_value,
        private_key_pem_encrypted: envelope.ciphertext,
        private_key_nonce: envelope.nonce,
        created_at: now,
        activates_at: now,
        expires_at: now + Duration::days(total),
        rotated_out_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use jsonwebtoken::DecodingKey;
    use std::sync::Mutex;

    #[derive(Default)]
    struct InMemorySigningKeyRepo {
        rows: Mutex<Vec<A2aSigningKeyRow>>,
    }

    #[async_trait]
    impl A2aSigningKeyRepository for InMemorySigningKeyRepo {
        async fn insert(&self, row: &A2aSigningKeyRow) -> Result<(), OrkError> {
            self.rows.lock().unwrap().push(row.clone());
            Ok(())
        }

        async fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<A2aSigningKeyRow>, OrkError> {
            let mut out: Vec<_> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.expires_at > now)
                .cloned()
                .collect();
            out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
            Ok(out)
        }

        async fn mark_rotated(&self, id: Uuid, at: DateTime<Utc>) -> Result<(), OrkError> {
            for r in self.rows.lock().unwrap().iter_mut() {
                if r.id == id {
                    r.rotated_out_at = Some(at);
                }
            }
            Ok(())
        }
    }

    fn kek() -> [u8; KEK_LEN] {
        encryption::derive_kek("unit-test-secret")
    }

    #[tokio::test]
    async fn sign_and_verify_against_published_jwk() {
        let repo: Arc<dyn A2aSigningKeyRepository> = Arc::new(InMemorySigningKeyRepo::default());
        let provider = JwksProvider::new(repo, kek(), RotationPolicy::default())
            .await
            .unwrap();
        provider.ensure_at_least_one(Utc::now()).await.unwrap();

        let body = b"{\"hello\":\"world\"}";
        let (jws, kid) = provider.sign_detached(body).await.unwrap();
        let parts: Vec<&str> = jws.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected detached header..signature, got {jws}"
        );
        let (header_b64, signature_b64) = (parts[0], parts[2]);
        let payload_b64 = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
        let signing_input = format!("{header_b64}.{payload_b64}");

        let jwks = provider.jwks().await;
        let key = jwks["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["kid"] == kid)
            .expect("kid missing from JWKS");
        let x = key["x"].as_str().unwrap();
        let y = key["y"].as_str().unwrap();
        let decoding = DecodingKey::from_ec_components(x, y).unwrap();

        let valid = jsonwebtoken::crypto::verify(
            signature_b64,
            signing_input.as_bytes(),
            &decoding,
            Algorithm::ES256,
        )
        .unwrap();
        assert!(valid, "JWS must verify against published JWK");
    }

    #[tokio::test]
    async fn rotation_creates_overlap_window() {
        let repo: Arc<dyn A2aSigningKeyRepository> = Arc::new(InMemorySigningKeyRepo::default());
        let provider = JwksProvider::new(repo, kek(), RotationPolicy::default())
            .await
            .unwrap();
        provider.ensure_at_least_one(Utc::now()).await.unwrap();
        let first_kid = provider.current_signer().await.unwrap().kid;

        let outcome = provider
            .rotate_if_due(Utc::now(), true)
            .await
            .unwrap()
            .expect("forced rotation must produce an outcome");
        assert_ne!(outcome.new_kid, first_kid);
        assert_eq!(outcome.previous_kid.as_deref(), Some(first_kid.as_str()));

        let jwks = provider.jwks().await;
        let kids: Vec<_> = jwks["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kid"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            kids.contains(&first_kid),
            "old key must remain in JWKS during overlap"
        );
        assert!(
            kids.contains(&outcome.new_kid),
            "new key must appear in JWKS"
        );

        // current_signer flips to the new key.
        let signer = provider.current_signer().await.unwrap();
        assert_eq!(signer.kid, outcome.new_kid);
    }

    #[tokio::test]
    async fn rotate_if_due_is_a_no_op_for_fresh_key() {
        let repo: Arc<dyn A2aSigningKeyRepository> = Arc::new(InMemorySigningKeyRepo::default());
        let provider = JwksProvider::new(repo, kek(), RotationPolicy::default())
            .await
            .unwrap();
        provider.ensure_at_least_one(Utc::now()).await.unwrap();
        let outcome = provider.rotate_if_due(Utc::now(), false).await.unwrap();
        assert!(outcome.is_none(), "fresh key should not rotate");
    }

    #[tokio::test]
    async fn rotate_if_due_fires_for_aged_key() {
        let repo: Arc<dyn A2aSigningKeyRepository> = Arc::new(InMemorySigningKeyRepo::default());
        let provider = JwksProvider::new(repo, kek(), RotationPolicy::default())
            .await
            .unwrap();
        let long_ago = Utc::now() - Duration::days(31);
        provider.ensure_at_least_one(long_ago).await.unwrap();
        let outcome = provider
            .rotate_if_due(Utc::now(), false)
            .await
            .unwrap()
            .expect("aged key should rotate");
        assert!(outcome.previous_kid.is_some());
    }
}
