//! Round-trip [`WorkflowSnapshotStore`](ork_core::ports::workflow_snapshot::WorkflowSnapshotStore) for Postgres (ADR-0050).

use ork_core::ports::workflow_snapshot::{RunStateBlob, SnapshotKey, WorkflowSnapshotStore};
use ork_persistence::postgres::{
    create_pool, workflow_snapshot_repo::PgWorkflowSnapshotRepository,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    create_pool(&url, 2).await.ok()
}

#[tokio::test]
async fn save_take_list_pending_mark_consumed_round_trip() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping workflow_snapshots round-trip test");
        return;
    };
    let repo = PgWorkflowSnapshotRepository::new(pool);
    let wf = format!("wf-snap-{}", Uuid::now_v7());
    let run_id = Uuid::now_v7();
    let key = SnapshotKey {
        workflow_id: wf.clone(),
        run_id,
        step_id: "step-1".into(),
        attempt: 1,
    };
    repo.save(
        key.clone(),
        json!({ "suspend": {}, "resume": null }),
        json!({ "type": "object" }),
        RunStateBlob(json!({ "pc": 0, "acc": null })),
    )
    .await
    .expect("save");

    let row = repo.take(key.clone()).await.expect("take").expect("row");
    assert_eq!(row.key.workflow_id, wf);

    let pending = repo.list_pending().await.expect("list_pending");
    assert!(
        pending.iter().any(|r| r.key.run_id == run_id),
        "expected pending row"
    );

    repo.mark_consumed(key.clone())
        .await
        .expect("mark_consumed");

    let pending2 = repo.list_pending().await.expect("list_pending2");
    assert!(!pending2.iter().any(|r| r.key.run_id == run_id));
}
