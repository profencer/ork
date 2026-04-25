//! Tiny axum receiver used by the ork demo (stage 7).
//!
//! Accepts ADR-0009 push notifications. ork-push (`crates/ork-push/src/worker.rs`)
//! sends the body as plain JSON and puts the **detached JWS** in headers:
//!
//!   - body                    : `application/json` task envelope
//!     (`{ task_id, tenant_id, state, occurred_at }`)
//!   - `X-A2A-Signature`       : compact JWS, `<protected>.<>.<signature>`
//!     where the empty middle segment denotes detached payload (RFC 7515 §A.5).
//!   - `X-A2A-Key-Id`          : kid that signed it (matches `/.well-known/jwks.json`)
//!   - `X-A2A-Timestamp`       : RFC3339 send-time
//!
//! For each delivery we:
//!
//! 1. Base64URL-decode the protected header from `X-A2A-Signature` and
//!    JSON-decode it to surface `{alg, kid}`.
//! 2. JSON-decode the request body (the task envelope).
//! 3. Pretty-print `{kid, alg, payload}` to stdout.
//! 4. Append to an in-process ring of the last `--max-history` deliveries and
//!    flush the ring to `--state-file` so the demo script can `cat` it after
//!    the run.
//!
//! We do NOT verify the JWS signature: the signing keys belong to ork-api
//! and the demo script will fetch them from `/.well-known/jwks.json` to
//! prove rotation works (stage 7 step 4). Verification would be a spec
//! compliance lap, not a tool that helps the audience understand the demo.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use clap::Parser;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(about = "Demo push-notification receiver")]
struct Args {
    /// Listen address (e.g. 127.0.0.1:8091).
    #[arg(long, env = "RECEIVER_ADDR", default_value = "127.0.0.1:8091")]
    addr: std::net::SocketAddr,

    /// Path to write the last `--max-history` deliveries (JSON array).
    #[arg(long, env = "RECEIVER_STATE_FILE", default_value = "demo/.last-hooks.json")]
    state_file: PathBuf,

    /// Number of deliveries kept in memory and mirrored to `--state-file`.
    #[arg(long, env = "RECEIVER_MAX_HISTORY", default_value_t = 10)]
    max_history: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Delivery {
    received_at: DateTime<Utc>,
    method: String,
    path: String,
    header: Value,
    payload: Value,
    raw: String,
}

#[derive(Clone)]
struct AppState {
    history: Arc<Mutex<Vec<Delivery>>>,
    state_file: PathBuf,
    max_history: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,demo_webhook_receiver=info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let state = AppState {
        history: Arc::new(Mutex::new(Vec::with_capacity(args.max_history))),
        state_file: args.state_file.clone(),
        max_history: args.max_history,
    };

    let app = Router::new()
        .route("/hook", post(receive_hook))
        .route("/last", get(get_last))
        .route("/health", get(health))
        .with_state(state);

    info!(addr = %args.addr, state_file = %args.state_file.display(),
          "demo webhook-receiver listening");
    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("webhook-receiver received Ctrl-C, shutting down");
}

async fn health() -> &'static str {
    "ok"
}

async fn receive_hook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let signature = headers
        .get("x-a2a-signature")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let kid_header = headers
        .get("x-a2a-key-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let timestamp = headers
        .get("x-a2a-timestamp")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let header_value = match signature.as_deref() {
        Some(jws) => match decode_protected_header(jws) {
            Ok(mut hdr) => {
                if let (Some(obj), Some(kid)) = (hdr.as_object_mut(), kid_header.as_ref()) {
                    obj.entry("kid".to_string())
                        .or_insert(Value::String(kid.clone()));
                }
                if let (Some(obj), Some(ts)) = (hdr.as_object_mut(), timestamp.as_ref()) {
                    obj.insert("ts".to_string(), Value::String(ts.clone()));
                }
                hdr
            }
            Err(e) => {
                warn!(error = %e, "could not decode X-A2A-Signature protected header");
                let mut obj = serde_json::Map::new();
                obj.insert("decode_error".into(), Value::String(e));
                if let Some(k) = &kid_header {
                    obj.insert("kid".into(), Value::String(k.clone()));
                }
                if let Some(t) = &timestamp {
                    obj.insert("ts".into(), Value::String(t.clone()));
                }
                Value::Object(obj)
            }
        },
        None => {
            // No signature header at all — surface that explicitly so the
            // operator can tell signing is misconfigured rather than silently
            // accepting unsigned bodies.
            let mut obj = serde_json::Map::new();
            obj.insert(
                "warning".into(),
                Value::String("no X-A2A-Signature header on request".into()),
            );
            if let Some(k) = &kid_header {
                obj.insert("kid".into(), Value::String(k.clone()));
            }
            if let Some(t) = &timestamp {
                obj.insert("ts".into(), Value::String(t.clone()));
            }
            Value::Object(obj)
        }
    };

    let payload_value: Value = serde_json::from_str(&body).unwrap_or_else(|_| Value::String(body.clone()));

    info!(kid = ?header_value.get("kid"), alg = ?header_value.get("alg"),
          "<-- push delivery received");
    println!("\n=== push delivery @ {} ===", Utc::now().to_rfc3339());
    println!(
        "header : {}",
        serde_json::to_string_pretty(&header_value).unwrap_or_default()
    );
    println!(
        "payload: {}",
        serde_json::to_string_pretty(&payload_value).unwrap_or_default()
    );

    let delivery = Delivery {
        received_at: Utc::now(),
        method: "POST".into(),
        path: "/hook".into(),
        header: header_value,
        payload: payload_value,
        raw: body,
    };
    push_history(&state, delivery);
    (StatusCode::ACCEPTED, "ok")
}

async fn get_last(State(state): State<AppState>) -> impl IntoResponse {
    let history = state.history.lock();
    Json(history.clone())
}

fn push_history(state: &AppState, delivery: Delivery) {
    let mut h = state.history.lock();
    h.push(delivery);
    while h.len() > state.max_history {
        h.remove(0);
    }
    if let Some(parent) = state.state_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(serialised) = serde_json::to_string_pretty(&*h) {
        if let Err(e) = std::fs::write(&state.state_file, serialised) {
            warn!(error = %e, file = %state.state_file.display(),
                  "failed to write state file");
        }
    }
}

/// Decode the protected header of a JWS compact-form string. Accepts both
/// attached form (`<header>.<payload>.<sig>`) and detached form
/// (`<header>..<sig>`, RFC 7515 §A.5) — we only ever look at the first
/// segment so the middle part can be empty.
fn decode_protected_header(jws: &str) -> Result<Value, String> {
    let header_b64 = jws
        .split('.')
        .next()
        .ok_or_else(|| "empty signature".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| format!("header base64: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("header json: {e}"))
}
