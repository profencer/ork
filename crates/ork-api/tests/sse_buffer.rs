//! ADR-0008 §`SSE bridge` — replay-buffer contract tests for the in-memory variant.
//! The Redis variant is exercised in higher-level integration tests; here we pin
//! the offset semantics and eviction window the bridge handler relies on.

use std::time::{Duration, SystemTime};

use ork_api::sse_buffer::{InMemorySseBuffer, ReplayEvent, SseBuffer};

#[tokio::test]
async fn append_and_replay_returns_events_after_offset() {
    let buf = InMemorySseBuffer::new(Duration::from_secs(60));
    buf.append(
        "task-1",
        ReplayEvent {
            id: 1,
            payload: b"a".to_vec(),
            at: SystemTime::now(),
        },
    )
    .await;
    buf.append(
        "task-1",
        ReplayEvent {
            id: 2,
            payload: b"b".to_vec(),
            at: SystemTime::now(),
        },
    )
    .await;
    let after = buf.replay("task-1", Some(1)).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, 2);
    assert_eq!(after[0].payload, b"b");
}

#[tokio::test]
async fn replay_without_offset_returns_all_events() {
    let buf = InMemorySseBuffer::new(Duration::from_secs(60));
    for id in 1..=3 {
        buf.append(
            "task-2",
            ReplayEvent {
                id,
                payload: vec![id as u8],
                at: SystemTime::now(),
            },
        )
        .await;
    }
    let all = buf.replay("task-2", None).await;
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, 1);
    assert_eq!(all[2].id, 3);
}

#[tokio::test]
async fn replay_for_unknown_task_returns_empty() {
    let buf = InMemorySseBuffer::new(Duration::from_secs(60));
    assert!(buf.replay("nope", None).await.is_empty());
    assert!(buf.replay("nope", Some(100)).await.is_empty());
}

#[tokio::test]
async fn evicts_after_window() {
    let buf = InMemorySseBuffer::new(Duration::from_millis(50));
    buf.append(
        "t",
        ReplayEvent {
            id: 1,
            payload: b"x".to_vec(),
            at: SystemTime::now() - Duration::from_secs(1),
        },
    )
    .await;
    buf.evict_expired().await;
    assert!(buf.replay("t", None).await.is_empty());
}

#[tokio::test]
async fn eviction_keeps_fresh_events() {
    let buf = InMemorySseBuffer::new(Duration::from_secs(60));
    buf.append(
        "t",
        ReplayEvent {
            id: 1,
            payload: b"old".to_vec(),
            at: SystemTime::now() - Duration::from_secs(120),
        },
    )
    .await;
    buf.append(
        "t",
        ReplayEvent {
            id: 2,
            payload: b"new".to_vec(),
            at: SystemTime::now(),
        },
    )
    .await;
    buf.evict_expired().await;
    let remaining = buf.replay("t", None).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, 2);
}
