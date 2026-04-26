//! ADR-0016 — pluggable `ArtifactStore` implementations (`fs`, `s3`, …).
//!
//! See [`ork_core::ports::artifact_store::ArtifactStore`] and `docs/adrs/0016-artifact-storage.md`.

pub mod chained;
mod scope_path;

pub use scope_path::{key_prefix_with_name, scope_prefix_path};

#[cfg(feature = "azblob")]
pub mod azblob;
#[cfg(feature = "fs")]
pub mod fs;
#[cfg(feature = "gcs")]
pub mod gcs;
#[cfg(feature = "s3")]
pub mod s3;
