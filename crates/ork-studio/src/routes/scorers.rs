//! `/studio/api/scorers*` — Studio Scorers panel data sources.
//!
//! v1 ships the *listing* aggregation: pass-rate, p50/p95, regression
//! count over the recent rows in
//! [`ScorerResultSink::list_recent`](ork_eval::live::ScorerResultSink).
//! The full time-series rendering is the SPA's job; this endpoint
//! returns enough headline metrics for the panel header.

use std::sync::Arc;

use axum::{Extension, Router, extract::Query, routing::get};
use ork_app::OrkApp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::{StudioEnvelope, ok};

pub fn routes() -> Router {
    Router::new()
        .route("/studio/api/scorers", get(list_bindings))
        .route("/studio/api/scorers/aggregate", get(aggregate))
}

#[derive(Debug, Serialize)]
struct ScorerBindingSummary {
    target: Value,
    scorer_id: String,
}

async fn list_bindings(
    Extension(app): Extension<Arc<OrkApp>>,
) -> StudioEnvelope<Vec<ScorerBindingSummary>> {
    let rows: Vec<ScorerBindingSummary> = app
        .scorers()
        .iter()
        .map(|b| ScorerBindingSummary {
            target: serde_json::to_value(&b.target).unwrap_or(Value::Null),
            scorer_id: b.spec.scorer().id().to_string(),
        })
        .collect();
    ok(rows)
}

#[derive(Debug, Deserialize)]
struct AggregateQuery {
    /// Maximum number of recent rows to consider. Capped at 5000.
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct AggregateRow {
    scorer_id: String,
    sample_count: usize,
    pass_rate: f64,
    p50: f64,
    p95: f64,
    regression_count: usize,
}

#[derive(Debug, Serialize)]
struct AggregateResponse {
    rows: Vec<AggregateRow>,
}

async fn aggregate(
    Extension(app): Extension<Arc<OrkApp>>,
    Query(q): Query<AggregateQuery>,
) -> StudioEnvelope<AggregateResponse> {
    let limit = q.limit.unwrap_or(1000).min(5000);
    let rows = app.scorer_sink().list_recent(limit).await;

    let mut by_scorer: std::collections::HashMap<String, Vec<f64>> =
        std::collections::HashMap::new();
    for r in rows {
        by_scorer
            .entry(r.scorer_id)
            .or_default()
            .push(f64::from(r.score));
    }

    let mut out: Vec<AggregateRow> = by_scorer
        .into_iter()
        .map(|(scorer_id, mut scores)| {
            let n = scores.len();
            scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let pass_threshold = 0.5_f64;
            let passes = scores.iter().filter(|s| **s >= pass_threshold).count();
            let pass_rate = if n == 0 {
                0.0
            } else {
                passes as f64 / n as f64
            };
            // Regressions: scores below the threshold (simple v1 heuristic;
            // the real ADR-0054 definition compares to a baseline corpus and
            // is owned by `ork eval`).
            let regression_count = n - passes;
            AggregateRow {
                scorer_id,
                sample_count: n,
                pass_rate,
                p50: percentile(&scores, 0.50),
                p95: percentile(&scores, 0.95),
                regression_count,
            }
        })
        .collect();
    out.sort_by(|a, b| a.scorer_id.cmp(&b.scorer_id));
    ok(AggregateResponse { rows: out })
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // Nearest-rank (Mastra ships the same on Studio; good enough for v1).
    let rank = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}
