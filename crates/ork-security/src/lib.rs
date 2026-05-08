//! Security primitives shared across ork crates (ADR-0020 §`Mesh trust`).
//!
//! This crate is intentionally narrow: it provides types and ports for
//! mesh-internal trust (the `X-Ork-Mesh-Token` shape between ork peers) and
//! later, in Phase C of ADR-0020, the [`KmsClient`](kms) trait + envelope
//! encryption primitives. It does NOT depend on the wire / persistence
//! crates so both `ork-api` (inbound verify) and
//! `ork-integrations::a2a_client` (outbound mint) can pull from it without
//! creating a dependency cycle.
//!
//! Phase B publishes:
//! - [`MeshClaims`] — the JWT body shape carried in `X-Ork-Mesh-Token`.
//! - [`MeshTokenSigner`] — port for minting and verifying mesh tokens.
//! - [`HmacMeshTokenSigner`] — default HS256 implementation backed by
//!   `auth.mesh_secret` (falls back to `auth.jwt_secret` for dev parity).
//! - [`intersect_scopes`] — caller-scope ∩ destination-card-accepted-scopes.
//!
//! Trust class / tier and the typed `TenantId` are re-used from
//! [`ork_common::auth`] so the wire shape is identical to what
//! [`ork_common::auth::AuthContext`] already carries on the inbound side
//! (Phase A).

pub mod audit;
pub mod kms;
pub mod mesh;
pub mod scopes;
pub mod tenant_cipher;

pub use kms::{Ciphertext, JwtSecretKekKms, KmsClient, KmsError};
pub use mesh::{
    HmacMeshTokenSigner, MeshClaims, MeshTokenSigner, MeshVerificationError, mesh_token_header,
};
pub use scopes::{ScopeChecker, intersect_scopes, scope_matches};
pub use tenant_cipher::{SealedField, TenantSecretsCipher};
