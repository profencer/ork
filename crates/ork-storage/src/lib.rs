//! ADR-0016 — pluggable `ArtifactStore` implementations (`fs`, `s3`, …).
//!
//! ADR-0021 §`Decision points` step 4 adds the [`scoped::ScopeCheckedArtifactStore`]
//! decorator that gates the trait surface on
//! `artifact:<scope>:<action>` scopes.
//!
//! See [`ork_core::ports::artifact_store::ArtifactStore`] and `docs/adrs/0016-artifact-storage.md`.

pub mod chained;
mod scope_path;
pub mod scoped;

pub use scope_path::{key_prefix_with_name, scope_prefix_path};
pub use scoped::ScopeCheckedArtifactStore;

#[cfg(feature = "azblob")]
pub mod azblob;
#[cfg(feature = "fs")]
pub mod fs;
#[cfg(feature = "gcs")]
pub mod gcs;
#[cfg(feature = "s3")]
pub mod s3;
