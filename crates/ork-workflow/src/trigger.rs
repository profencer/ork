//! Cron / webhook triggers for code-first workflows (ADR [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub enum TriggerSpec {
    Cron { expr: String, tz: String },
    Webhook { path: String },
}

/// Facade matching ADR naming (`Trigger::cron(...)`).
pub struct Trigger;

impl Trigger {
    #[must_use]
    pub fn cron(expr: impl Into<String>, tz: impl Into<String>) -> TriggerSpec {
        TriggerSpec::Cron {
            expr: expr.into(),
            tz: tz.into(),
        }
    }

    #[must_use]
    pub fn webhook(path: impl Into<String>) -> TriggerSpec {
        TriggerSpec::Webhook { path: path.into() }
    }
}

/// Mock-clock friendly scheduler: call [`Self::tick`] with an advanced [`DateTime`].
pub struct SchedulerService {
    crons: Vec<(String, Schedule)>,
    last_fire: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl SchedulerService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            crons: Vec::new(),
            last_fire: Mutex::new(HashMap::new()),
        }
    }

    /// Register a workflow id with a cron expression (standard 5-field cron).
    pub fn register_cron(
        &mut self,
        workflow_id: impl Into<String>,
        expr: &str,
    ) -> Result<(), String> {
        let sched = Schedule::from_str(expr).map_err(|e| e.to_string())?;
        self.crons.push((workflow_id.into(), sched));
        Ok(())
    }

    /// `true` when no cron workflows are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.crons.is_empty()
    }

    /// Returns workflow ids whose next scheduled time falls in `(last_fire, now]`
    /// (mock-clock friendly).
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut fired = Vec::new();
        let mut last = self.last_fire.lock().await;
        for (wid, sched) in &self.crons {
            let from = last
                .get(wid)
                .copied()
                .unwrap_or_else(|| now - chrono::Duration::minutes(1));
            let mut it = sched.after(&from);
            let Some(next) = it.next() else {
                continue;
            };
            if next <= now {
                fired.push(wid.clone());
                last.insert(wid.clone(), next);
            }
        }
        fired
    }
}

impl Default for SchedulerService {
    fn default() -> Self {
        Self::new()
    }
}
