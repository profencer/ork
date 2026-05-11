//! `demo-studio-tour` — post-pivot showcase boot.
//!
//! Composes a full `OrkApp` (2 agents, 2 tools, 1 workflow, libsql
//! memory, 2 scorer bindings) and serves it over the ADR-0056 auto
//! REST/SSE surface plus ADR-0055 Studio at `/studio`. The demo
//! binary also mounts its own `POST /demo/seed` route via
//! `axum::Router::merge(...)` so reviewers can hit "Demo data" and
//! watch Studio's panels light up without driving every interaction
//! by hand.
//!
//! Bind: `127.0.0.1:4111`. Open `http://127.0.0.1:4111/studio` after
//! the "listening" log line appears.

mod agents;
mod seed;
mod tools;
mod workflows;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Extension, Router, routing::post};
use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioConfig};
use ork_core::ports::memory_store::{MemoryOptions, SemanticRecallConfig};
use ork_eval::Sampling;
use ork_eval::ScorerSpec;
use ork_eval::scorers::exact_match;
use ork_eval::spec::ScorerTarget;
use ork_memory::Memory;
use tokio::sync::oneshot;

const LISTEN_HOST: &str = "127.0.0.1";
const LISTEN_PORT: u16 = 4111;

/// Pinned demo tenant. Every request to the auto router and Studio
/// resolves to this tenant via `ServerConfig::default_tenant`, so the
/// browser doesn't need to send `X-Ork-Tenant` on each fetch.
const DEMO_TENANT_ID: &str = "11111111-1111-1111-1111-111111111111";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,reqwest=warn")),
        )
        .init();

    eprintln!(
        "demo-studio-tour: post-pivot showcase booting on http://{LISTEN_HOST}:{LISTEN_PORT}"
    );

    // LLM provider — resolved from env (see agents::build_llm_provider).
    let llm = agents::build_llm_provider().context("resolve LLM provider")?;

    // libsql memory at `./demo-studio-tour.db` so reviewers can poke
    // around with `sqlite3` if they want to. The Memory panel uses
    // this backend's `delete_thread` per ADR-0053.
    let db_path =
        std::env::var("ORK_DEMO_DB_PATH").unwrap_or_else(|_| "demo-studio-tour.db".into());
    // ADR-0053 semantic recall defaults to ON, which requires an embedder.
    // The demo doesn't ship one (an embedder would also need a separate
    // API key); turn recall off so the libsql backend boots without
    // demanding `ORK_DEMO_EMBEDDER_KEY`. Working memory + last_messages
    // are still injected into agent prompts.
    let memory_options = MemoryOptions {
        semantic_recall: SemanticRecallConfig {
            enabled: false,
            ..SemanticRecallConfig::default()
        },
        ..MemoryOptions::default()
    };
    let memory = Memory::libsql(format!("file:{db_path}"))
        .options(memory_options)
        .open()
        .await
        .context("open libsql memory")?;

    let concierge = agents::concierge_agent(Arc::clone(&llm))?;
    let analyst = agents::analyst_agent(Arc::clone(&llm))?;

    let cfg = ServerConfig {
        host: LISTEN_HOST.into(),
        port: LISTEN_PORT,
        studio: StudioConfig::Enabled,
        // ADR-0056 §`Tenant header`: a missing `X-Ork-Tenant` returns 400
        // by default. Browsers can't ergonomically attach the header
        // before paint, so the demo pins a single tenant id for every
        // request. Real deployments wire JWT-derived tenants per
        // ADR-0020.
        default_tenant: Some(DEMO_TENANT_ID.into()),
        ..ServerConfig::default()
    };

    let app = OrkApp::builder()
        .server(cfg.clone())
        .agent(concierge)
        .agent(analyst)
        .tool(tools::now_tool())
        .tool(tools::dice_tool())
        .workflow(workflows::feedback_triage_workflow())
        .memory_arc(memory)
        .scorer(
            ScorerTarget::agent("concierge"),
            ScorerSpec::offline(exact_match().expected_field("answer").build()),
        )
        .scorer(
            ScorerTarget::agent("concierge"),
            ScorerSpec::live(
                exact_match().expected_field("answer").build(),
                Sampling::Ratio { rate: 0.5 },
            ),
        )
        .build()
        .context("build OrkApp")?;

    let app_arc = Arc::new(app.clone());

    // Compose the router by hand so we can merge:
    //   1. ADR-0056 auto-generated REST/SSE surface (`ork-api`).
    //   2. ADR-0055 Studio + `/studio/api/*` (`ork-studio`).
    //   3. The demo's own `POST /demo/seed` route.
    let mut router: Router = ork_api::router_for(&app, &cfg);
    if let Some(studio) = ork_studio::router(&app, &cfg) {
        router = router.merge(studio);
    }
    let demo_routes = Router::new()
        .route("/demo/seed", post(seed::seed))
        .layer(Extension(Arc::clone(&app_arc)));
    router = router.merge(demo_routes);

    let listen_addr = format!("{LISTEN_HOST}:{LISTEN_PORT}");
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    let local_addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });

    eprintln!(
        "demo-studio-tour: serving on http://{local_addr}\n  \
         open http://{local_addr}/studio  (the dashboard)\n  \
         seed:  curl -X POST http://{local_addr}/demo/seed | jq"
    );

    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "demo-studio-tour: ctrl_c watcher");
        }
        let _ = shutdown_tx.send(());
    });

    serve.await.context("axum::serve")?;
    Ok(())
}
