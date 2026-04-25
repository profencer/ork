//! Envelope encryption for ADR-0009 signing keys.
//!
//! The Key Encryption Key (KEK) is derived deterministically from
//! `config.auth.jwt_secret` using HKDF-SHA256 with the salt
//! [`KEK_SALT`]. Any deployment with the same `jwt_secret` derives the
//! same KEK, so a freshly-restored database can be decrypted as long as
//! the secret is in `ORK__AUTH__JWT_SECRET`.
//!
//! Plaintext private keys never leave this module: `seal` produces an
//! [`Envelope`] of `(ciphertext, nonce)` and `open` returns the decoded
//! plaintext only on a successful AEAD verification — any tamper of either
//! side is rejected as [`EncryptionError::Open`].
//!
//! AES-256-GCM is the natural envelope cipher for "small secret, infrequent
//! access" workloads: 96-bit nonces (the `aead`/AES-GCM standard) sized for
//! random generation per seal call, and the GCM tag detects tampering.
//!
//! See ADR-0009 §`Real envelope encryption`.

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;

/// HKDF salt that pins the derivation to "ork push KEK v1".
///
/// Bumping the suffix forces re-encryption of every signing key (and any
/// future caller that adopts the helper). Today there is exactly one
/// version so no migration tooling is needed.
pub const KEK_SALT: &[u8] = b"ork.a2a.push.kek.v1";

/// HKDF info parameter — context-binds the derived key to the push module so
/// the same `jwt_secret` can be reused for unrelated KEKs without overlap.
pub const KEK_INFO: &[u8] = b"ork-push/signing-key-encryption";

/// Length of the derived KEK (AES-256 → 32 bytes).
pub const KEK_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("HKDF expand failed: {0}")]
    Hkdf(String),
    #[error("AEAD seal failed: {0}")]
    Seal(String),
    #[error("AEAD open failed (key/ciphertext/nonce mismatch or tampered): {0}")]
    Open(String),
    #[error("invalid nonce length: expected 12, got {0}")]
    NonceLen(usize),
}

/// Sealed AEAD output. `nonce` is 12 bytes; we keep it as `Vec<u8>` so the
/// Postgres binding is straightforward (`BYTEA`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

/// Derive the 32-byte KEK from `jwt_secret` using HKDF-SHA256 with [`KEK_SALT`].
///
/// Pure function — same input, same output — so tests and CLI flows can
/// produce a KEK without any service wiring.
#[must_use]
pub fn derive_kek(jwt_secret: &str) -> [u8; KEK_LEN] {
    let hk = Hkdf::<Sha256>::new(Some(KEK_SALT), jwt_secret.as_bytes());
    let mut out = [0u8; KEK_LEN];
    hk.expand(KEK_INFO, &mut out)
        .expect("HKDF expand of 32 bytes from SHA-256 cannot fail");
    out
}

/// Encrypt `plaintext` under `kek` with a freshly-sampled 96-bit nonce.
pub fn seal(plaintext: &[u8], kek: &[u8; KEK_LEN]) -> Result<Envelope, EncryptionError> {
    let key = Key::<Aes256Gcm>::from_slice(kek);
    let cipher = Aes256Gcm::new(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: KEK_INFO,
            },
        )
        .map_err(|e| EncryptionError::Seal(e.to_string()))?;
    Ok(Envelope {
        ciphertext,
        nonce: nonce.to_vec(),
    })
}

/// Decrypt `env` under `kek`. Returns `Err` for any tamper, key mismatch,
/// or wrong-length nonce.
pub fn open(env: &Envelope, kek: &[u8; KEK_LEN]) -> Result<Vec<u8>, EncryptionError> {
    if env.nonce.len() != 12 {
        return Err(EncryptionError::NonceLen(env.nonce.len()));
    }
    let key = Key::<Aes256Gcm>::from_slice(kek);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&env.nonce);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &env.ciphertext,
                aad: KEK_INFO,
            },
        )
        .map_err(|e| EncryptionError::Open(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_kek_is_deterministic() {
        let a = derive_kek("super-secret-jwt");
        let b = derive_kek("super-secret-jwt");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_kek_changes_with_secret() {
        let a = derive_kek("alpha");
        let b = derive_kek("beta");
        assert_ne!(a, b);
    }

    #[test]
    fn seal_open_round_trip() {
        let kek = derive_kek("test-secret");
        let plaintext = b"-----BEGIN PRIVATE KEY-----\nfoo\n-----END PRIVATE KEY-----\n";
        let env = seal(plaintext, &kek).expect("seal");
        let out = open(&env, &kek).expect("open");
        assert_eq!(out, plaintext);
    }

    #[test]
    fn open_detects_ciphertext_tamper() {
        let kek = derive_kek("test-secret");
        let mut env = seal(b"plaintext", &kek).expect("seal");
        env.ciphertext[0] ^= 0x01;
        let err = open(&env, &kek).expect_err("tampered ciphertext must fail");
        assert!(matches!(err, EncryptionError::Open(_)));
    }

    #[test]
    fn open_detects_nonce_tamper() {
        let kek = derive_kek("test-secret");
        let mut env = seal(b"plaintext", &kek).expect("seal");
        env.nonce[0] ^= 0x01;
        let err = open(&env, &kek).expect_err("tampered nonce must fail");
        assert!(matches!(err, EncryptionError::Open(_)));
    }

    #[test]
    fn open_detects_wrong_kek() {
        let kek_a = derive_kek("alpha");
        let kek_b = derive_kek("beta");
        let env = seal(b"plaintext", &kek_a).expect("seal");
        let err = open(&env, &kek_b).expect_err("wrong KEK must fail");
        assert!(matches!(err, EncryptionError::Open(_)));
    }

    #[test]
    fn open_rejects_wrong_nonce_length() {
        let kek = derive_kek("test-secret");
        let env = Envelope {
            ciphertext: vec![0; 16],
            nonce: vec![0; 11],
        };
        let err = open(&env, &kek).expect_err("short nonce must fail");
        assert!(matches!(err, EncryptionError::NonceLen(11)));
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let kek = derive_kek("test-secret");
        let a = seal(b"plaintext", &kek).expect("seal");
        let b = seal(b"plaintext", &kek).expect("seal");
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }
}
