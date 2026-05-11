//! `POST /demo/seed` — the "Demo data" button.
//!
//! Studio v1 ships a fixed-set SPA (no user-extensible panels until a
//! follow-up ADR), so the "button" here is a custom POST route mounted
//! by the demo binary's `main.rs` alongside `/studio` and `/api/...`.
//! A reviewer can either:
//!
//! - `curl -X POST http://127.0.0.1:4111/demo/seed` (CLI), or
//! - hit it via Studio's Swagger UI / browser tab (ADR-0056 §`Swagger`).
//!
//! Seeding writes:
//! - 2 memory threads with sample chat history for the
//!   `demo-resource` resource id so Studio's Memory panel renders.
//! - 12 synthetic `exact_match` scorer rows (8 passing + 4 regressing)
//!   so the Scorers panel has aggregate data on first load.

use std::sync::Arc;

use axum::{Extension, Json, http::StatusCode, response::IntoResponse};
use ork_a2a::{ResourceId, ThreadId};
use ork_app::OrkApp;
use ork_common::types::TenantId;
use ork_core::ports::llm::ChatMessage;
use ork_core::ports::memory_store::MemoryContext;
use ork_core::ports::scorer::{RunId, RunKind};
use ork_eval::live::ScoredRow;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SeedReport {
    pub memory_threads_seeded: usize,
    pub memory_messages_seeded: usize,
    pub scorer_rows_seeded: usize,
}

pub async fn seed(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    let tenant = TenantId::new();
    let resource = ResourceId(uuid::Uuid::new_v4());

    let mut threads_seeded = 0usize;
    let mut messages_seeded = 0usize;

    if let Some(memory) = app.memory() {
        let seeds: [(&str, &[(&str, &str)]); 2] = [
            (
                "Onboarding chat",
                &[
                    ("user", "Hi, what can this demo do?"),
                    (
                        "assistant",
                        "I can tell you the time, roll dice, and run a daily briefing workflow.",
                    ),
                    ("user", "Roll me 3d20."),
                ],
            ),
            (
                "Briefing dry-run",
                &[
                    ("user", "Brief me on the ork pivot."),
                    (
                        "assistant",
                        "ADR-0048 moves us code-first; Studio (ADR-0055) is the dev surface.",
                    ),
                ],
            ),
        ];

        for (_label, msgs) in &seeds {
            let thread = ThreadId::new();
            let ctx = MemoryContext {
                tenant_id: tenant,
                resource_id: resource,
                thread_id: thread,
                agent_id: "concierge".into(),
            };
            for (role, text) in *msgs {
                let m = match *role {
                    "user" => ChatMessage::user(*text),
                    "assistant" => ChatMessage::assistant(*text, vec![]),
                    other => {
                        tracing::warn!(role = other, "demo seed: skipping unknown role");
                        continue;
                    }
                };
                if let Err(e) = memory.append_message(&ctx, m).await {
                    tracing::warn!(error = %e, "demo seed: append_message failed");
                    continue;
                }
                messages_seeded += 1;
            }
            threads_seeded += 1;
        }
    } else {
        tracing::warn!("demo seed: no memory backend configured; skipping thread seeds");
    }

    // Synthetic scorer rows so the Scorers panel has aggregate data on
    // first open. 12 rows across two scorer ids; 8 pass (≥ 0.5), 4 fail.
    let sink = app.scorer_sink();
    let mut scorer_rows = 0usize;
    let synthetic = [
        ("exact_match", 1.0_f32),
        ("exact_match", 1.0),
        ("exact_match", 1.0),
        ("exact_match", 0.0),
        ("exact_match", 1.0),
        ("exact_match", 1.0),
        ("latency_under", 1.0),
        ("latency_under", 1.0),
        ("latency_under", 0.0),
        ("latency_under", 1.0),
        ("latency_under", 0.0),
        ("latency_under", 0.0),
    ];
    for (scorer_id, score) in synthetic {
        let row = ScoredRow {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("concierge".into()),
            workflow_id: None,
            scorer_id: scorer_id.into(),
            score,
            label: None,
            rationale: None,
            details: serde_json::json!({"seeded": true}),
            scorer_duration_ms: 0,
            sampled_via: "demo-seed".into(),
            tenant_id: tenant,
            judge_model: None,
            judge_input_tokens: None,
            judge_output_tokens: None,
        };
        sink.record(row).await;
        scorer_rows += 1;
    }

    (
        StatusCode::OK,
        Json(SeedReport {
            memory_threads_seeded: threads_seeded,
            memory_messages_seeded: messages_seeded,
            scorer_rows_seeded: scorer_rows,
        }),
    )
}
