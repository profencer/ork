use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::workspace::{CodeSearchHit, RepoWorkspace, RepositorySpec};
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use tracing::info;

/// Expand a leading `~/` using `$HOME`.
pub fn expand_cache_dir(path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Local shallow clones (`git` CLI) + ripgrep-backed search.
///
/// Concurrency: tool catalogues frequently expose `code_search`, `read_file`,
/// `list_tree` together, and the LLM happily fires several of them in parallel
/// against the same repo on the same turn. Each call goes through
/// [`GitRepoWorkspace::ensure_clone`], which without serialisation would race
/// inside [`ensure_clone_inner`] — both callers see no `.git`, both invoke
/// `git clone`, the second one fails with `git`'s `fatal: could not create
/// work tree dir '...': File exists` (exit 128). To dodge that we keep a
/// per-clone-path async [`tokio::sync::Mutex`] in [`Self::clone_locks`] and
/// hold it across the `spawn_blocking` that runs `ensure_clone_inner`. The
/// outer [`std::sync::Mutex`] only guards map insertion (held for microseconds);
/// the contended one is the per-path inner mutex.
#[derive(Clone)]
pub struct GitRepoWorkspace {
    cache_dir: PathBuf,
    clone_depth: u32,
    specs: Vec<RepositorySpec>,
    clone_locks: Arc<StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>>>,
}

impl GitRepoWorkspace {
    pub fn new(cache_dir: PathBuf, clone_depth: u32, specs: Vec<RepositorySpec>) -> Self {
        Self {
            cache_dir,
            clone_depth,
            specs,
            clone_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Per-path mutex for [`Self::ensure_clone`]. Cheap to hold the outer
    /// std::Mutex across the `entry().or_insert_with()` call because we never
    /// `.await` while it's locked.
    fn clone_lock(&self, path: &Path) -> Arc<TokioMutex<()>> {
        let mut guard = self
            .clone_locks
            .lock()
            .expect("clone_locks std::Mutex poisoned");
        guard
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    fn repo_path(&self, tenant_id: TenantId, name: &str) -> Result<PathBuf, OrkError> {
        let spec = self
            .specs
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| OrkError::NotFound(format!("unknown repository '{name}'")))?;
        Ok(self.cache_dir.join(tenant_id.to_string()).join(&spec.name))
    }

    fn spec(&self, name: &str) -> Result<&RepositorySpec, OrkError> {
        self.specs
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| OrkError::NotFound(format!("unknown repository '{name}'")))
    }
}

#[async_trait]
impl RepoWorkspace for GitRepoWorkspace {
    fn list_specs(&self) -> Vec<RepositorySpec> {
        self.specs.clone()
    }

    async fn ensure_clone(&self, tenant_id: TenantId, name: &str) -> Result<String, OrkError> {
        let path = self.repo_path(tenant_id, name)?;
        let path_for_task = path.clone();
        let spec = self.spec(name)?.clone();
        let depth = self.clone_depth;

        // Hold the per-path mutex across the spawn_blocking so concurrent
        // `code_search` / `read_file` / `list_tree` calls for the same repo
        // serialise on the clone/fetch step instead of stepping on each
        // other inside `git`.
        let lock = self.clone_lock(&path);
        let _guard = lock.lock().await;

        tokio::task::spawn_blocking(move || {
            ensure_clone_inner(&path_for_task, &spec.url, &spec.default_branch, depth)
        })
        .await
        .map_err(|e| OrkError::Internal(format!("join ensure_clone: {e}")))?
        .map(|_| {
            let p = path.to_string_lossy().into_owned();
            info!(repository = %name, path = %p, "git: clone/fetch ready");
            p
        })
    }

    async fn code_search(
        &self,
        tenant_id: TenantId,
        name: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<CodeSearchHit>, OrkError> {
        let root = self.ensure_clone(tenant_id, name).await?;
        let root_path = PathBuf::from(root);
        let q = query.to_string();
        let max = top_k.clamp(1, 500);

        tokio::task::spawn_blocking(move || run_ripgrep(&root_path, &q, max))
            .await
            .map_err(|e| OrkError::Internal(format!("join code_search: {e}")))?
    }

    async fn read_file(
        &self,
        tenant_id: TenantId,
        name: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<String, OrkError> {
        let root = self.ensure_clone(tenant_id, name).await?;
        let root_path = PathBuf::from(root);
        let rel = path.to_string();
        let cap = max_bytes.clamp(256, 2 * 1024 * 1024);

        tokio::task::spawn_blocking(move || read_file_inner(&root_path, &rel, cap))
            .await
            .map_err(|e| OrkError::Internal(format!("join read_file: {e}")))?
    }

    async fn list_tree(
        &self,
        tenant_id: TenantId,
        name: &str,
        prefix: &str,
        max_entries: usize,
    ) -> Result<Vec<String>, OrkError> {
        let root = self.ensure_clone(tenant_id, name).await?;
        let root_path = PathBuf::from(root);
        let prefix = prefix.to_string();
        let max = max_entries.clamp(1, 10_000);

        tokio::task::spawn_blocking(move || list_tree_inner(&root_path, &prefix, max))
            .await
            .map_err(|e| OrkError::Internal(format!("join list_tree: {e}")))?
    }
}

fn git_cmd() -> Command {
    Command::new("git")
}

fn ensure_clone_inner(path: &Path, url: &str, branch: &str, depth: u32) -> Result<(), OrkError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| OrkError::Validation("repository path is not valid UTF-8".into()))?;

    if path
        .join(".git")
        .try_exists()
        .map_err(|e| OrkError::Integration(e.to_string()))?
    {
        let status = git_cmd()
            .args([
                "-C",
                path_str,
                "fetch",
                "--depth",
                &depth.to_string(),
                "origin",
                branch,
            ])
            .status()
            .map_err(|e| {
                OrkError::Integration(format!(
                    "failed to run `git fetch` (is `git` installed?): {e}"
                ))
            })?;
        if !status.success() {
            return Err(OrkError::Integration(format!(
                "`git fetch` exited with {:?}",
                status.code()
            )));
        }

        let remote_ref = format!("origin/{branch}");
        let status = git_cmd()
            .args(["-C", path_str, "reset", "--hard", &remote_ref])
            .status()
            .map_err(|e| OrkError::Integration(e.to_string()))?;
        if !status.success() {
            return Err(OrkError::Integration(format!(
                "`git reset --hard {}` exited with {:?}",
                remote_ref,
                status.code()
            )));
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| OrkError::Integration(e.to_string()))?;
    }

    let output = git_cmd()
        .args([
            "clone",
            "--depth",
            &depth.to_string(),
            "--branch",
            branch,
            url,
            path_str,
        ])
        .output()
        .map_err(|e| {
            OrkError::Integration(format!(
                "failed to run `git clone` (is `git` installed?): {e}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(OrkError::Integration(format!(
            "`git clone {url} -> {path_str}` exited with {:?}: {}",
            output.status.code(),
            stderr.trim()
        )));
    }
    Ok(())
}

fn run_ripgrep(root: &Path, query: &str, top_k: usize) -> Result<Vec<CodeSearchHit>, OrkError> {
    let mut child = Command::new("rg")
        .args([
            "--json",
            "-S",
            "--max-count",
            &format!("{}", top_k.saturating_mul(3).max(top_k)),
            query,
            ".",
        ])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            OrkError::Integration(format!("failed to spawn ripgrep (is `rg` installed?): {e}"))
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| OrkError::Integration("rg stdout missing".into()))?;

    let reader = std::io::BufReader::new(stdout);
    let mut hits = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| OrkError::Integration(e.to_string()))?;
        if hits.len() >= top_k {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => continue,
        };
        let path_text = data
            .pointer("/path/text")
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let line_number = data
            .get("line_number")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32;
        let line_content = data
            .pointer("/lines/text")
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();

        hits.push(CodeSearchHit {
            path: path_text.to_string(),
            line_number,
            line: line_content,
        });
    }

    let status = child
        .wait()
        .map_err(|e| OrkError::Integration(e.to_string()))?;
    if !status.success() && hits.is_empty() && status.code() != Some(1) {
        return Err(OrkError::Integration(format!(
            "rg exited with {:?}",
            status.code()
        )));
    }

    Ok(hits)
}

fn resolve_under_root(root: &Path, rel: &str) -> Result<PathBuf, OrkError> {
    let rel = rel.trim_start_matches(['/', '\\']);
    if rel.contains("..") {
        return Err(OrkError::Validation("path must not contain '..'".into()));
    }
    let joined = root.join(rel);
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let full = joined.canonicalize().unwrap_or_else(|_| joined.clone());
    if !full.starts_with(&root_canon) {
        return Err(OrkError::Validation("path escapes repository root".into()));
    }
    Ok(full)
}

fn read_file_inner(root: &Path, rel: &str, max_bytes: usize) -> Result<String, OrkError> {
    let path = resolve_under_root(root, rel)?;
    let bytes = std::fs::read(&path).map_err(|e| OrkError::Integration(e.to_string()))?;
    let slice = if bytes.len() > max_bytes {
        &bytes[..max_bytes]
    } else {
        &bytes[..]
    };
    Ok(String::from_utf8_lossy(slice).into_owned())
}

fn list_tree_inner(root: &Path, prefix: &str, max_entries: usize) -> Result<Vec<String>, OrkError> {
    let base = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        resolve_under_root(root, prefix)?
    };

    let mut out = Vec::new();
    let mut q = VecDeque::new();
    q.push_back(base);

    while let Some(dir) = q.pop_front() {
        let entries = std::fs::read_dir(&dir).map_err(|e| OrkError::Integration(e.to_string()))?;
        for ent in entries {
            let ent = ent.map_err(|e| OrkError::Integration(e.to_string()))?;
            let p = ent.path();
            let rel = p.strip_prefix(root).unwrap_or(&p);
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            out.push(rel_s);
            if out.len() >= max_entries {
                return Ok(out);
            }
            let ft = ent
                .file_type()
                .map_err(|e| OrkError::Integration(e.to_string()))?;
            if ft.is_dir() {
                q.push_back(p);
            }
        }
    }

    out.sort();
    Ok(out)
}
