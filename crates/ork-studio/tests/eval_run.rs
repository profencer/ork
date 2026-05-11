//! ADR-0055 AC #10: `POST /studio/api/evals/run` returns an
//! [`EvalReport`](ork_eval::runner::EvalReport) against a local
//! 3-example JSONL fixture; the report includes passed / failed /
//! regression counts.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioConfig};
use tower::ServiceExt;

#[tokio::test]
async fn eval_run_returns_report_for_three_example_fixture() {
    // Copy the bundled fixture next to a tempdir so the eval runner
    // can write `studio-eval-report.json` next to it without
    // contaminating the source tree.
    let td = tempfile::tempdir().expect("tempdir");
    let dataset_path = td.path().join("weather-3.jsonl");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("weather-3.jsonl");
    std::fs::copy(&src, &dataset_path).expect("copy fixture");

    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            studio: StudioConfig::Enabled,
            ..ServerConfig::default()
        })
        .build()
        .expect("build app");
    let cfg = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        studio: StudioConfig::Enabled,
        default_tenant: Some("11111111-1111-1111-1111-111111111111".into()),
        ..ServerConfig::default()
    };
    let router = ork_studio::router(&app, &cfg).expect("studio enabled");

    let body = serde_json::json!({
        "dataset": dataset_path.to_string_lossy(),
        "agent": "weather",
        "echo_from": "answer",
        "scorers": ["exact_match=answer"],
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/studio/api/evals/run")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /studio/api/evals/run failed"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(
        json.get("studio_api_version").and_then(|v| v.as_u64()),
        Some(1),
        "envelope missing version"
    );

    // Fixture: ex-1 + ex-2 match (echo_from=answer equals expected.answer),
    // ex-3 mismatches ("rainy" vs "snowy"). The runner scores each example
    // → report.examples = 3, passed = 2, failed = 1.
    let report = json.pointer("/data/report").expect("data.report");
    assert_eq!(
        report.get("examples").and_then(|v| v.as_u64()),
        Some(3),
        "report: {report}"
    );
    assert_eq!(
        report.get("passed").and_then(|v| v.as_u64()),
        Some(2),
        "report: {report}"
    );
    assert_eq!(
        report.get("failed").and_then(|v| v.as_u64()),
        Some(1),
        "report: {report}"
    );
    assert!(
        report
            .get("by_scorer")
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.contains_key("exact_match")),
        "by_scorer missing exact_match: {report}"
    );
    assert!(
        report
            .get("regressions")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty()),
        "no baseline -> regressions must be empty: {report}"
    );
}
