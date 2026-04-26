//! Early-phase embed resolution (ADR-0015).

use chrono::{TimeZone, Utc};
use ork_common::types::TenantId;
use ork_core::embeds::{EmbedContext, EmbedLimits, EmbedRegistry, resolve_early};
use uuid::Uuid;

fn ctx(tenant: TenantId) -> EmbedContext {
    let mut c = EmbedContext::with_limits(
        tenant,
        None,
        None,
        None,
        Utc.with_ymd_and_hms(2026, 4, 26, 12, 0, 0).unwrap(),
        [("foo".to_string(), "bar".to_string())]
            .into_iter()
            .collect(),
        &EmbedLimits::default(),
    );
    c.depth = 0;
    c
}

#[tokio::test]
async fn math_and_uuid() {
    let reg = EmbedRegistry::with_builtins();
    let lim = EmbedLimits::default();
    let t = ctx(TenantId(Uuid::new_v4()));
    let s = resolve_early("«math:2*3 | int» «uuid»", &t, &reg, &lim)
        .await
        .expect("ok");
    assert!(s.contains('6'), "math, got {s}");
    // Two UUIDs → many hyphens; one from uuid embed is enough
    assert!(s.matches('-').count() >= 4, "expected uuid, got {s}");
}

#[tokio::test]
async fn var_lookup() {
    let reg = EmbedRegistry::with_builtins();
    let t = ctx(TenantId(Uuid::new_v4()));
    let s = resolve_early("«var:foo»", &t, &reg, &EmbedLimits::default())
        .await
        .unwrap();
    assert_eq!(s, "bar");
}

#[tokio::test]
async fn max_embeds_cap() {
    let reg = EmbedRegistry::with_builtins();
    let lim = EmbedLimits {
        max_embeds_per_request: 0,
        ..Default::default()
    };
    let t = ctx(TenantId(Uuid::new_v4()));
    let err = resolve_early("«uuid»", &t, &reg, &lim)
        .await
        .expect_err("cap");
    assert!(err.to_string().contains("max") || err.to_string().contains("embed"));
}

#[tokio::test]
async fn unknown_type_passthrough() {
    let reg = EmbedRegistry::with_builtins();
    let t = ctx(TenantId(Uuid::new_v4()));
    let s = resolve_early(
        "«not_a_registered_embed_type:foo»",
        &t,
        &reg,
        &EmbedLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(s, "«not_a_registered_embed_type:foo»");
}
