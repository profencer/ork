//! Shared key-prefix helpers for all backends (ADR-0016 §`Scope`).

use ork_core::ports::artifact_store::ArtifactScope;

/// Returns `<tenant_uuid>/<context_uuid>/` (no trailing name yet).
/// Most backends prefix logical names under this.
#[must_use]
pub fn scope_prefix_path(scope: &ArtifactScope) -> String {
    format!("{}/{}", scope.tenant_id.0, scope.context_key_uuid())
}

/// Full relative key: `<tenant>/<context>/<name>` (logical `name` as given).
#[must_use]
pub fn key_prefix_with_name(scope: &ArtifactScope, name: &str) -> String {
    format!(
        "{}/{}/{}",
        scope.tenant_id.0,
        scope.context_key_uuid(),
        name
    )
}
