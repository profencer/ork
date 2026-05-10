//! `notify-debouncer-mini` wrapper for `ork dev`. Default roots:
//! `<workspace>/src/` and `<workspace>/workflow-templates/`. The user
//! can extend with `--watch <path>`.
//!
//! ADR-0057 §`Acceptance criteria` #9: debounce ≥ 200 ms so saving in
//! bursts does not trigger a rebuild storm.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::{
    DebounceEventResult, Debouncer, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub paths: Vec<PathBuf>,
}

/// Spawn a watcher and return both a receiver and the debouncer handle
/// (must be kept alive for the watcher to remain active).
pub fn spawn_watcher(
    roots: &[PathBuf],
    debounce: Duration,
) -> Result<(mpsc::Receiver<ChangeEvent>, Debouncer<RecommendedWatcher>)> {
    let (tx, rx) = mpsc::channel::<ChangeEvent>(8);

    let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| {
        match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events
                    .into_iter()
                    .map(|e| e.path)
                    .filter(|p| !is_ignored(p))
                    .collect();
                if paths.is_empty() {
                    return;
                }
                // Bounded channel: if the supervisor is busy with a build,
                // drop events under back-pressure rather than buffer
                // unbounded.
                let _ = tx.try_send(ChangeEvent { paths });
            }
            Err(err) => {
                tracing::warn!("ork dev watcher error: {err:?}");
            }
        }
    })
    .context("ork dev: build notify debouncer")?;

    for root in roots {
        if !root.exists() {
            continue;
        }
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("ork dev: watch {}", root.display()))?;
    }

    Ok((rx, debouncer))
}

fn is_ignored(p: &Path) -> bool {
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "target" || s == ".git" || s == ".DS_Store" || s.starts_with('.')
    })
}
