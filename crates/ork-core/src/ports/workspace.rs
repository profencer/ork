use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ork_common::error::OrkError;
use ork_common::types::TenantId;

/// Declared repository available for local clone / code search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositorySpec {
    pub name: String,
    pub url: String,
    pub default_branch: String,
}

/// One line match from ripgrep / code search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSearchHit {
    pub path: String,
    pub line_number: u32,
    pub line: String,
}

/// Local git workspace: shallow clones, search, and read files under tenant-scoped cache dirs.
#[async_trait]
pub trait RepoWorkspace: Send + Sync {
    /// All configured repositories (for list_repos tool).
    fn list_specs(&self) -> Vec<RepositorySpec>;

    /// Ensure repo is cloned/up to date; returns absolute path on disk.
    async fn ensure_clone(&self, tenant_id: TenantId, name: &str) -> Result<String, OrkError>;

    /// Run a search (e.g. ripgrep) in the given repo working tree.
    async fn code_search(
        &self,
        tenant_id: TenantId,
        name: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<CodeSearchHit>, OrkError>;

    /// Read a file relative to repo root (must stay within root).
    async fn read_file(
        &self,
        tenant_id: TenantId,
        name: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<String, OrkError>;

    /// List paths under `prefix` (relative), up to `max_entries`.
    async fn list_tree(
        &self,
        tenant_id: TenantId,
        name: &str,
        prefix: &str,
        max_entries: usize,
    ) -> Result<Vec<String>, OrkError>;
}
