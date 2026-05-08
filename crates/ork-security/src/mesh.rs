//! ADR-0020 §`Mesh trust — JWT claims and propagation`: the JWT body
//! carried in the `X-Ork-Mesh-Token` header on every outbound A2A request
//! between ork peers.
//!
//! `X-Ork-Mesh-Token` is **separate** from `Authorization`: ork-to-ork
//! traffic still carries whatever caller credential mTLS / OAuth provides
//! (terminated at Kong), and the mesh token rides alongside as a
//! short-lived attestation of "this is what the originator's
//! `AuthContext` looked like at the moment the call was issued". Inbound
//! servers prefer the mesh-token claims over the bearer-derived
//! `AuthContext` when both are present (Phase B4 wires the override in
//! `auth_middleware`).
//!
//! HS256 / shared-secret today; ADR-0020 §`Open questions` records the
//! migration to RS256/ES256 + JWKS as a follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode, errors::ErrorKind,
};
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// HTTP header that carries the mesh token. Defined here so callers
/// (`ork-api` middleware, `ork-integrations::a2a_client`) all use the
/// exact same string and never typo it.
pub const MESH_TOKEN_HEADER: &str = "X-Ork-Mesh-Token";

/// Convenience for callers that want the header name as a `&'static str`
/// without depending on a specific HTTP crate.
#[must_use]
pub fn mesh_token_header() -> &'static str {
    MESH_TOKEN_HEADER
}

/// JWT body carried in `X-Ork-Mesh-Token`.
///
/// Field shape mirrors the bearer-token `JwtClaims` defined in
/// `ork-common::auth` so a downstream that already knows how to read an
/// `AuthContext` can consume mesh-attested claims with minimal
/// translation. `tenant_chain` is serialised as `tid_chain` to match the
/// bearer token wire format.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshClaims {
    /// JWT subject — typically `agent:<local-id>` for ork-to-ork tokens.
    pub sub: String,
    /// Originator tenant id (the leaf tenant of `tid_chain`).
    #[serde(rename = "tenant_id", with = "tenant_id_str")]
    pub tenant_id: TenantId,
    /// Ordered tenant chain. Length 1 ⇔ no trust-boundary crossing yet.
    #[serde(default, rename = "tid_chain", with = "tenant_chain_str")]
    pub tenant_chain: Vec<TenantId>,
    /// Caller's scopes ∩ destination card's `accepted_scopes`. The
    /// receiver enforces these as if they were the caller's bearer
    /// scopes for this hop.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub trust_tier: TrustTier,
    #[serde(default)]
    pub trust_class: TrustClass,
    /// Local ork agent id whose ork minted this token (when
    /// `trust_class == Agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub exp: i64,
    pub iat: i64,
    pub iss: String,
    pub aud: String,
}

impl MeshClaims {
    /// Build claims with `iat = now`, `exp = now + ttl`. Caller still
    /// owns `iss`/`aud` (they typically come from `auth.mesh_iss` /
    /// destination-card-derived `aud`).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sub: String,
        tenant_id: TenantId,
        tenant_chain: Vec<TenantId>,
        scopes: Vec<String>,
        trust_tier: TrustTier,
        trust_class: TrustClass,
        agent_id: Option<String>,
        iss: String,
        aud: String,
        ttl: chrono::Duration,
    ) -> Self {
        let iat = Utc::now();
        let exp = iat + ttl;
        Self {
            sub,
            tenant_id,
            tenant_chain,
            scopes,
            trust_tier,
            trust_class,
            agent_id,
            exp: exp.timestamp(),
            iat: iat.timestamp(),
            iss,
            aud,
        }
    }

    /// Convenience accessor — returns `iat` as a typed `DateTime`.
    #[must_use]
    pub fn issued_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.iat, 0)
    }
}

/// Errors specific to mesh token verification. Wrapped into
/// [`OrkError::Auth`] when surfaced to handlers.
#[derive(Debug, Error)]
pub enum MeshVerificationError {
    #[error("mesh token expired")]
    Expired,
    #[error("mesh token issuer mismatch (expected {expected}, got {actual})")]
    IssuerMismatch { expected: String, actual: String },
    #[error("mesh token audience mismatch (expected {expected}, got {actual})")]
    AudienceMismatch { expected: String, actual: String },
    #[error("malformed mesh token: {0}")]
    Malformed(String),
}

impl From<MeshVerificationError> for OrkError {
    fn from(err: MeshVerificationError) -> Self {
        OrkError::Unauthorized(err.to_string())
    }
}

/// Port for minting and verifying mesh tokens. `Arc<dyn MeshTokenSigner>`
/// is what `A2aRemoteAgent` and `auth_middleware` consume so we can swap
/// the underlying algorithm (HS256 today, RS256 / KMS-backed tomorrow)
/// without touching the call sites.
///
/// `issuer` / `audience` are exposed so callers can stamp them into
/// [`MeshClaims`] without each having to know how the signer was
/// configured. A caller building claims should always use
/// `signer.issuer()` / `signer.audience()` rather than reading config
/// separately, so that "I minted this with signer X" and "I verify
/// with signer X" can never disagree on the values.
#[async_trait]
pub trait MeshTokenSigner: Send + Sync {
    fn issuer(&self) -> &str;
    fn audience(&self) -> &str;
    async fn mint(&self, claims: MeshClaims) -> Result<String, OrkError>;
    async fn verify(&self, token: &str) -> Result<MeshClaims, OrkError>;
}

/// Default HS256 implementation. The secret comes from
/// `auth.mesh_secret` (with a fallback to `auth.jwt_secret` for dev
/// parity with the bearer token signer). `iss` / `aud` are validated
/// against the configured values; `exp` is enforced by `jsonwebtoken`.
pub struct HmacMeshTokenSigner {
    encode_key: EncodingKey,
    decode_key: DecodingKey,
    issuer: String,
    audience: String,
    /// Held only so `Debug` of the wrapping `Arc<dyn>` doesn't accidentally
    /// expose the secret if anyone derives `Debug` further up the tree.
    #[allow(dead_code)]
    secret: SecretString,
}

impl std::fmt::Debug for HmacMeshTokenSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacMeshTokenSigner")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl HmacMeshTokenSigner {
    #[must_use]
    pub fn new(secret: SecretString, issuer: String, audience: String) -> Self {
        let bytes = secret.expose_secret().as_bytes();
        let encode_key = EncodingKey::from_secret(bytes);
        let decode_key = DecodingKey::from_secret(bytes);
        Self {
            encode_key,
            decode_key,
            issuer,
            audience,
            secret,
        }
    }

    /// Wrap into an `Arc<dyn MeshTokenSigner>` so wiring into
    /// `A2aRemoteAgentBuilder` / `AppState` is a one-liner.
    #[must_use]
    pub fn arc(self) -> Arc<dyn MeshTokenSigner> {
        Arc::new(self)
    }
}

#[async_trait]
impl MeshTokenSigner for HmacMeshTokenSigner {
    fn issuer(&self) -> &str {
        &self.issuer
    }

    fn audience(&self) -> &str {
        &self.audience
    }

    async fn mint(&self, claims: MeshClaims) -> Result<String, OrkError> {
        encode(&Header::new(Algorithm::HS256), &claims, &self.encode_key)
            .map_err(|e| OrkError::Internal(format!("mint mesh token: {e}")))
    }

    async fn verify(&self, token: &str) -> Result<MeshClaims, OrkError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(std::slice::from_ref(&self.issuer));
        validation.set_audience(std::slice::from_ref(&self.audience));
        // `exp` is validated by default; we explicitly require iat too so
        // a forged "no iat" token doesn't pass.
        validation.set_required_spec_claims(&["exp", "iat", "iss", "aud", "sub"]);

        match decode::<MeshClaims>(token, &self.decode_key, &validation) {
            Ok(data) => Ok(data.claims),
            Err(e) => match e.kind() {
                ErrorKind::ExpiredSignature => Err(MeshVerificationError::Expired.into()),
                ErrorKind::InvalidIssuer => Err(MeshVerificationError::IssuerMismatch {
                    expected: self.issuer.clone(),
                    actual: "<redacted>".into(),
                }
                .into()),
                ErrorKind::InvalidAudience => Err(MeshVerificationError::AudienceMismatch {
                    expected: self.audience.clone(),
                    actual: "<redacted>".into(),
                }
                .into()),
                _ => Err(MeshVerificationError::Malformed(e.to_string()).into()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// serde helpers — mirror the `JwtClaims` wire format (TenantId as String).
// ---------------------------------------------------------------------------

mod tenant_id_str {
    use super::{TenantId, Uuid};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &TenantId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.0.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<TenantId, D::Error> {
        let raw = String::deserialize(d)?;
        Uuid::parse_str(&raw)
            .map(TenantId)
            .map_err(serde::de::Error::custom)
    }
}

mod tenant_chain_str {
    use super::{TenantId, Uuid};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(chain: &[TenantId], s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(chain.len()))?;
        for tid in chain {
            seq.serialize_element(&tid.0.to_string())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<TenantId>, D::Error> {
        let raw: Vec<String> = Vec::deserialize(d)?;
        raw.into_iter()
            .map(|s| {
                Uuid::parse_str(&s)
                    .map(TenantId)
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn signer() -> HmacMeshTokenSigner {
        HmacMeshTokenSigner::new(
            SecretString::from("super-secret-test-key"),
            "ork-test".into(),
            "ork-mesh".into(),
        )
    }

    fn sample_claims() -> MeshClaims {
        let tid = TenantId(Uuid::now_v7());
        MeshClaims::new(
            "agent:planner".into(),
            tid,
            vec![tid],
            vec!["agent:reviewer:invoke".into()],
            TrustTier::Internal,
            TrustClass::Agent,
            Some("planner".into()),
            "ork-test".into(),
            "ork-mesh".into(),
            Duration::seconds(60),
        )
    }

    #[tokio::test]
    async fn mint_then_verify_roundtrips() {
        let s = signer();
        let claims = sample_claims();
        let token = s.mint(claims.clone()).await.expect("mint");
        let back = s.verify(&token).await.expect("verify");
        assert_eq!(back, claims);
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let s = signer();
        let mut claims = sample_claims();
        claims.iat -= 3600;
        claims.exp -= 3000; // 50 min ago, well past exp
        let token = s.mint(claims).await.expect("mint");
        let err = s.verify(&token).await.expect_err("should reject expired");
        assert!(format!("{err}").to_lowercase().contains("expired"));
    }

    #[tokio::test]
    async fn issuer_mismatch_rejected() {
        let signer_mint = HmacMeshTokenSigner::new(
            SecretString::from("k"),
            "wrong-iss".into(),
            "ork-mesh".into(),
        );
        let signer_verify = HmacMeshTokenSigner::new(
            SecretString::from("k"),
            "right-iss".into(),
            "ork-mesh".into(),
        );
        let mut claims = sample_claims();
        claims.iss = "wrong-iss".into();
        let token = signer_mint.mint(claims).await.expect("mint");
        signer_verify
            .verify(&token)
            .await
            .expect_err("issuer mismatch must be rejected");
    }

    #[tokio::test]
    async fn audience_mismatch_rejected() {
        let signer_mint =
            HmacMeshTokenSigner::new(SecretString::from("k"), "iss".into(), "wrong-aud".into());
        let signer_verify =
            HmacMeshTokenSigner::new(SecretString::from("k"), "iss".into(), "ork-mesh".into());
        let mut claims = sample_claims();
        claims.iss = "iss".into();
        claims.aud = "wrong-aud".into();
        let token = signer_mint.mint(claims).await.expect("mint");
        signer_verify
            .verify(&token)
            .await
            .expect_err("audience mismatch must be rejected");
    }

    #[tokio::test]
    async fn wrong_secret_rejected() {
        let s1 =
            HmacMeshTokenSigner::new(SecretString::from("k1"), "iss".into(), "ork-mesh".into());
        let s2 =
            HmacMeshTokenSigner::new(SecretString::from("k2"), "iss".into(), "ork-mesh".into());
        let mut c = sample_claims();
        c.iss = "iss".into();
        let token = s1.mint(c).await.expect("mint");
        s2.verify(&token).await.expect_err("wrong secret rejected");
    }

    #[test]
    fn debug_redacts_secret() {
        let s = signer();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("<redacted>"), "debug must redact: {dbg}");
        assert!(!dbg.contains("super-secret-test-key"));
    }

    #[test]
    fn header_constant_stable() {
        assert_eq!(mesh_token_header(), "X-Ork-Mesh-Token");
    }
}
