//! `ork inspect <target>` — print the [`AppManifest`] for a binary or a
//! running server. ADR-0057 §`ork inspect`.
//!
//! Two target shapes:
//! - **Path**: spawn the binary with `ORK_INSPECT_MANIFEST=1`; the
//!   binary's `OrkApp::serve()` early-exits and prints JSON to stdout.
//!   ADR-0057 wires the early-exit hook in `crates/ork-app/src/app.rs`.
//! - **URL**: GET `<base>/api/manifest` (mounted by ADR-0056 in
//!   `crates/ork-api/src/routes/auto/manifest.rs`).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

#[derive(Args)]
pub struct InspectArgs {
    /// Path to a built ork binary, OR an HTTP base URL (http(s)://...).
    pub target: String,

    /// Output format. `json` is the default; `table` renders a compact
    /// human-readable summary suitable for piping into pagers.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    pub format: Format,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Format {
    Json,
    Table,
}

pub async fn run(args: InspectArgs) -> Result<()> {
    let json = if looks_like_url(&args.target) {
        fetch_via_http(&args.target).await?
    } else {
        spawn_and_capture(PathBuf::from(&args.target)).await?
    };

    let manifest: serde_json::Value =
        serde_json::from_str(&json).context("ork inspect: target returned non-JSON manifest")?;

    match args.format {
        Format::Json => {
            // Re-pretty-print so URL responses (which may be compact) format consistently.
            let pretty = serde_json::to_string_pretty(&manifest)?;
            println!("{pretty}");
        }
        Format::Table => render_table(&manifest),
    }
    Ok(())
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

async fn fetch_via_http(base: &str) -> Result<String> {
    let url = if base.ends_with("/api/manifest") {
        base.to_string()
    } else {
        format!("{}/api/manifest", base.trim_end_matches('/'))
    };
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "ork inspect: GET {url} returned status {} — is the server running and is the \
             X-Ork-Tenant default set in ServerConfig?",
            resp.status()
        );
    }
    resp.text().await.context("read manifest body")
}

async fn spawn_and_capture(bin: PathBuf) -> Result<String> {
    if !bin.exists() {
        bail!(
            "ork inspect: target {} does not exist (pass a path to a built binary or an http(s):// URL)",
            bin.display()
        );
    }
    let output = tokio::process::Command::new(&bin)
        .env("ORK_INSPECT_MANIFEST", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawn {}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ork inspect: {} exited with status {}; stderr:\n{}",
            bin.display(),
            output.status,
            stderr
        );
    }
    String::from_utf8(output.stdout).context("ork inspect: manifest stdout was not valid UTF-8")
}

fn render_table(manifest: &serde_json::Value) {
    let env = manifest
        .get("environment")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let ork_version = manifest
        .get("ork_version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let built_at = manifest
        .get("built_at")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!("environment : {env}");
    println!("ork version : {ork_version}");
    println!("built at    : {built_at}");
    print_count(manifest, "agents");
    print_count(manifest, "workflows");
    print_count(manifest, "tools");
    print_count(manifest, "mcp_servers");
    print_count(manifest, "scorers");
    print_ids(manifest, "agents");
    print_ids(manifest, "workflows");
    print_ids(manifest, "tools");
}

fn print_count(manifest: &serde_json::Value, key: &str) {
    let n = manifest
        .get(key)
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    println!("{key:<11} : {n}");
}

fn print_ids(manifest: &serde_json::Value, key: &str) {
    let Some(arr) = manifest.get(key).and_then(|v| v.as_array()) else {
        return;
    };
    if arr.is_empty() {
        return;
    }
    println!("{key} ids:");
    for entry in arr {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("(missing id)");
        println!("  - {id}");
    }
}
