//! Mock-clockable cron ticks (ADR-0050).

use chrono::{TimeZone, Utc};
use ork_workflow::SchedulerService;

#[tokio::test]
async fn fires_within_two_ticks_advancing_clock() {
    let mut sched = SchedulerService::new();
    // Every hour at minute 0, second 0.
    sched
        .register_cron("wf-1", "0 0 * * * *")
        .expect("valid cron");
    let t0 = Utc.with_ymd_and_hms(2026, 5, 2, 12, 30, 0).unwrap();
    assert!(sched.tick(t0).await.is_empty());
    let t1 = Utc.with_ymd_and_hms(2026, 5, 2, 13, 0, 1).unwrap();
    let fired = sched.tick(t1).await;
    assert_eq!(fired, vec!["wf-1"]);
}
