//! Regression test for the `git clone` race in
//! [`ork_integrations::workspace::GitRepoWorkspace::ensure_clone`].
//!
//! Stage-4 of the demo repeatedly hit
//!
//! ```text
//! integration error: `git clone …` exited with Some(128):
//!   fatal: could not create work tree dir '…/anthropic-sdk-typescript': File exists
//! ```
//!
//! because the researcher's tool catalogue exposes `code_search`, `read_file`
//! and `list_tree` together; the LLM cheerfully fires several of them in
//! parallel against the same repo on the same turn. Both callers walked
//! through `ensure_clone_inner`, both observed no `.git` dir, both invoked
//! `git clone`, and the loser failed.
//!
//! Post-fix [`GitRepoWorkspace`] keeps a per-clone-path async mutex and
//! holds it across the `spawn_blocking` that runs the clone/fetch step,
//! so the second caller observes the first one's clone and falls into
//! the fast-path fetch-and-reset branch instead.

use std::process::Command;
use std::sync::Arc;

use ork_common::types::TenantId;
use ork_core::ports::workspace::{RepoWorkspace, RepositorySpec};
use ork_integrations::workspace::GitRepoWorkspace;

#[tokio::test]
async fn parallel_ensure_clone_for_same_repo_serialises_and_succeeds() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: `git` not installed");
        return;
    }

    // Stage scratch inside the workspace target dir so writes don't trip
    // sandboxed CI environments that forbid `/tmp/.git/hooks` etc. Falls
    // back to `CARGO_TARGET_TMPDIR` if the env var is set, otherwise the
    // crate manifest dir's `target/test-tmp/` (which is workspace-local
    // and `.gitignored` by `target/`).
    let scratch_root = std::env::var_os("ORK_TEST_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("test-tmp")
        });
    let scratch = scratch_root.join(format!("ork-workspace-test-{}", ork_a2a::TaskId::new()));
    let source = scratch.join("source");
    let cache = scratch.join("cache");
    std::fs::create_dir_all(&source).expect("mkdir source");
    std::fs::create_dir_all(&cache).expect("mkdir cache");

    // Bootstrap a tiny `main`-only repo. Using a local `file://` URL keeps
    // the test offline and fast.
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.email", "t@t"][..],
        &["config", "user.name", "t"][..],
        &["config", "commit.gpgsign", "false"][..],
    ] {
        let st = Command::new("git")
            .args(args)
            .current_dir(&source)
            .status()
            .expect("spawn git");
        assert!(st.success(), "git {args:?} failed");
    }
    std::fs::write(source.join("README"), "hi").unwrap();
    for args in [
        &["add", "."][..],
        &["commit", "-m", "init", "--no-gpg-sign"][..],
    ] {
        let st = Command::new("git")
            .args(args)
            .current_dir(&source)
            .status()
            .expect("spawn git");
        assert!(st.success(), "git {args:?} failed");
    }

    let url = format!("file://{}", source.display());
    let ws = Arc::new(GitRepoWorkspace::new(
        cache.clone(),
        1,
        vec![RepositorySpec {
            name: "demo".into(),
            url: url.clone(),
            default_branch: "main".into(),
        }],
    ));
    let tenant = TenantId::new();

    // Fan out N parallel callers for the same (tenant, repo). Without the
    // per-path mutex at least one of these races inside `git clone`.
    let mut handles = Vec::new();
    for _ in 0..5 {
        let ws = ws.clone();
        handles.push(tokio::spawn(async move {
            ws.ensure_clone(tenant, "demo").await
        }));
    }
    let results = futures::future::join_all(handles).await;
    for (i, r) in results.into_iter().enumerate() {
        let outcome = r.expect("join handle");
        outcome.unwrap_or_else(|e| panic!("parallel ensure_clone[{i}] must succeed; got {e:?}"));
    }

    let _ = std::fs::remove_dir_all(&scratch);
}
