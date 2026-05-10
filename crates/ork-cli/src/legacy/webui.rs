//! `ork legacy webui` — Web UI (ADR-0017): Vite dev server for
//! `client/webui/frontend`. ADR-0057 §`Legacy subcommands`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum WebuiCommand {
    /// Run `pnpm dev` in `client/webui/frontend` (set `WEBUI_DEV_PROXY` for ork-server).
    Dev {
        /// Vite dev server port.
        #[arg(long, default_value_t = 5173u16)]
        vite_port: u16,
    },
}

pub async fn run(cmd: WebuiCommand) -> Result<()> {
    match cmd {
        WebuiCommand::Dev { vite_port } => dev(vite_port).await,
    }
}

async fn dev(vite_port: u16) -> Result<()> {
    let fe = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/webui/frontend"
    ));
    if !fe.join("package.json").exists() {
        bail!(
            "Web UI package not found at {} (init client/webui/frontend first; see ADR-0017)",
            fe.display()
        );
    }
    let fe_str = fe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path {}", fe.display()))?;
    let webui = format!("http://127.0.0.1:{vite_port}");
    eprintln!("Starting Vite in {fe_str} (port {vite_port})…");
    eprintln!(
        "Then ork-server with WEBUI_DEV_PROXY={webui} and ADR-0017 config overlay (Ctrl-C stops both)."
    );
    eprintln!();

    let mut pnpm = tokio::process::Command::new("pnpm")
        .arg("--dir")
        .arg(fe_str)
        .arg("dev")
        .arg("--")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(vite_port.to_string())
        .arg("--strictPort")
        .kill_on_drop(true)
        .spawn()
        .context("spawn pnpm (install with `pnpm install` in the frontend dir if missing)")?;

    let vite_addr = format!("127.0.0.1:{vite_port}");
    let mut vite_ready = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(&vite_addr).await.is_ok() {
            vite_ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !vite_ready {
        eprintln!(
            "warning: Vite is not listening on {vite_addr} after ~20s; reload the browser if the first load fails"
        );
    }

    let ork_server = find_ork_server_binary()?;
    let webui_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/webui-dev.toml");
    eprintln!("Spawning {ork_server:?}…");
    let mut cmd = tokio::process::Command::new(&ork_server);
    cmd.env("WEBUI_DEV_PROXY", &webui);
    if std::env::var("ORK_A2A_PUBLIC_BASE").is_err() {
        cmd.env("ORK_A2A_PUBLIC_BASE", "http://127.0.0.1:8080");
    }
    if let Ok(abs) = webui_toml.canonicalize()
        && abs.is_file()
    {
        eprintln!(
            "Using ORK_CONFIG_EXTRA={} (enables `[[gateways]]` type=webui)",
            abs.display()
        );
        cmd.env("ORK_CONFIG_EXTRA", abs);
    }
    let mut server = cmd.kill_on_drop(true).spawn().with_context(|| {
        format!(
            "spawn ork-server at {} — build with `cargo build -p ork-api` or set ORK_SERVER_BIN",
            ork_server.display()
        )
    })?;

    tokio::select! {
        status = server.wait() => {
            let _ = pnpm.start_kill();
            let status = status.context("wait for ork-server")?;
            if !status.success() {
                bail!("ork-server exited with status {status}");
            }
        }
        r = pnpm.wait() => {
            let _ = server.start_kill();
            let r = r.context("wait for pnpm")?;
            if !r.success() {
                bail!("pnpm dev exited with status {r}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            let _ = server.start_kill();
            let _ = pnpm.start_kill();
        }
    }
    Ok(())
}

fn find_ork_server_binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ORK_SERVER_BIN") {
        let pb: PathBuf = p.into();
        if pb.is_file() {
            return Ok(pb);
        }
        bail!("ORK_SERVER_BIN is set but is not a file: {}", pb.display());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("ork-server");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "ork-server not found next to the `ork` binary; build with `cargo build -p ork-api` or set ORK_SERVER_BIN"
    );
}
