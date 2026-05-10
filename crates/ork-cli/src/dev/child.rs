//! Spawn the user binary, forward stdout/stderr with prefixes, wait for
//! readiness via `/readyz`, kill gracefully on demand.
//!
//! ADR-0057 §`ork dev`: hot reload is *binary restart*; this module is
//! the per-run lifecycle around one child process.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

pub struct AppChild {
    child: Child,
    port: u16,
    forward: Vec<tokio::task::JoinHandle<()>>,
}

impl AppChild {
    pub async fn spawn(bin: &Path, port: u16) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.env("PORT", port.to_string());
        cmd.env("ORK_DEV", "1");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("ork dev: spawn {}", bin.display()))?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_task = tokio::spawn(forward_lines(stdout, "[app stdout]"));
        let stderr_task = tokio::spawn(forward_lines_err(stderr, "[app stderr]"));

        Ok(Self {
            child,
            port,
            forward: vec![stdout_task, stderr_task],
        })
    }

    /// Polls `GET http://127.0.0.1:<port>/readyz` until 200 OK or the
    /// budget elapses. Returns Err if the child exits before ready.
    pub async fn await_ready(&mut self, budget: Duration) -> Result<()> {
        let url = format!("http://127.0.0.1:{}/readyz", self.port);
        let started = Instant::now();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("build readyz client")?;
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "ork dev: child exited before ready (status {status}); check the prefixed \
                     stderr forwarded above"
                );
            }
            if let Ok(resp) = client.get(&url).send().await
                && resp.status().is_success()
            {
                return Ok(());
            }
            if started.elapsed() >= budget {
                bail!(
                    "ork dev: child did not respond 200 to {url} within {} ms; the binary may \
                     have crashed silently or be bound to a different port (try setting PORT)",
                    budget.as_millis()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Best-effort graceful shutdown:
    /// - Unix: SIGTERM via `nix::sys::signal::kill`, wait up to `grace`,
    ///   then `start_kill()`.
    /// - Windows: `start_kill()` directly (no SIGTERM equivalent reachable
    ///   from `tokio::process::Child` without extra crates).
    pub async fn terminate(mut self, grace: Duration) -> Result<std::process::ExitStatus> {
        self.send_terminate_signal();
        let waited = tokio::time::timeout(grace, self.child.wait()).await;
        let status = match waited {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(anyhow!("ork dev: wait child after SIGTERM: {e}")),
            Err(_) => {
                let _ = self.child.start_kill();
                self.child
                    .wait()
                    .await
                    .context("ork dev: wait child after SIGKILL")?
            }
        };
        for handle in self.forward.drain(..) {
            handle.abort();
        }
        Ok(status)
    }

    // Reviewer n2: keep both arms `&mut self` so a future refactor that
    // moves the call site to a `&self` method doesn't silently break the
    // Windows path.
    #[cfg(unix)]
    fn send_terminate_signal(&mut self) {
        if let Some(pid) = self.child.id() {
            // Best effort; if the process already exited the kill is harmless.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    #[cfg(not(unix))]
    fn send_terminate_signal(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn forward_lines(stdout: tokio::process::ChildStdout, prefix: &'static str) {
    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        println!("{prefix} {line}");
    }
}

async fn forward_lines_err(stderr: tokio::process::ChildStderr, prefix: &'static str) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        eprintln!("{prefix} {line}");
    }
}
