//! `cargo build` invocation + artifact harvesting for `ork dev` / `ork build`.
//!
//! Streams `--message-format=json-render-diagnostics` so we can pick up
//! the produced binary path from the `compiler-artifact` message
//! without reconstructing `target/<profile>/<name>` paths ourselves.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug)]
pub enum BuildOutcome {
    Success {
        artifact: PathBuf,
        /// Captured cargo stderr (compiler diagnostics, etc.). Useful for
        /// surfacing warnings even on success.
        stderr: String,
    },
    Failed {
        stderr: String,
    },
}

/// Runs `cargo build [--release] --bin <bin> --message-format=json-render-diagnostics`
/// in `workspace_root`. Returns the compiled artifact path on success.
pub async fn cargo_build(workspace_root: &Path, bin: &str, profile: &str) -> Result<BuildOutcome> {
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("build");
    if profile == "release" {
        cmd.arg("--release");
    }
    cmd.arg("--bin").arg(bin);
    cmd.arg("--message-format=json-render-diagnostics");
    cmd.current_dir(workspace_root);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Reviewer M2: ensure the cargo subprocess is killed if the supervisor
    // task is dropped (e.g. on Ctrl-C); otherwise a stray cargo process
    // keeps holding `target/.cargo-lock` after `ork dev` exits.
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn cargo build --bin {bin}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let stdout_task = tokio::spawn(harvest_artifact(stdout, bin.to_string()));
    let stderr_task = tokio::spawn(collect_to_string(stderr));

    let status = child
        .wait()
        .await
        .with_context(|| format!("wait for cargo build --bin {bin}"))?;
    let artifact = stdout_task.await.context("artifact harvester join")??;
    let stderr = stderr_task.await.context("stderr collector join")??;

    if !status.success() {
        return Ok(BuildOutcome::Failed { stderr });
    }
    let Some(artifact) = artifact else {
        return Ok(BuildOutcome::Failed {
            stderr: format!(
                "cargo build succeeded but emitted no artifact for --bin {bin}\n{stderr}"
            ),
        });
    };
    Ok(BuildOutcome::Success { artifact, stderr })
}

async fn harvest_artifact(
    stdout: tokio::process::ChildStdout,
    bin: String,
) -> Result<Option<PathBuf>> {
    let mut reader = BufReader::new(stdout).lines();
    let mut last: Option<PathBuf> = None;
    while let Some(line) = reader.next_line().await? {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let reason = value.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        if reason != "compiler-artifact" {
            continue;
        }
        let target = value.get("target");
        let target_name = target
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kinds = target
            .and_then(|t| t.get("kind"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let is_bin_kind = kinds
            .iter()
            .any(|k| k.as_str().map(|s| s == "bin").unwrap_or(false));
        if !is_bin_kind || target_name != bin {
            continue;
        }
        if let Some(exe) = value.get("executable").and_then(|v| v.as_str()) {
            last = Some(PathBuf::from(exe));
        }
    }
    Ok(last)
}

async fn collect_to_string(stderr: tokio::process::ChildStderr) -> Result<String> {
    let mut buf = String::new();
    let mut reader = BufReader::new(stderr).lines();
    while let Some(line) = reader.next_line().await? {
        buf.push_str(&line);
        buf.push('\n');
    }
    Ok(buf)
}
