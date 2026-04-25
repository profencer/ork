use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use ork_common::types::{TenantId, WorkflowId};
use tokio::sync::RwLock;
use tracing::info;

use crate::models::workflow::WorkflowTrigger;

/// Tracks scheduled workflows and determines when they should fire.
pub struct WorkflowScheduler {
    schedules: Arc<RwLock<HashMap<(TenantId, WorkflowId), ScheduleEntry>>>,
}

struct ScheduleEntry {
    _cron_expr: String,
    schedule: cron::Schedule,
}

impl WorkflowScheduler {
    pub fn new() -> Self {
        Self {
            schedules: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(
        &self,
        tenant_id: TenantId,
        workflow_id: WorkflowId,
        trigger: &WorkflowTrigger,
    ) -> Result<(), String> {
        if let WorkflowTrigger::Schedule { cron: cron_expr } = trigger {
            let schedule: cron::Schedule = cron_expr
                .parse()
                .map_err(|e| format!("invalid cron expression: {e}"))?;

            let mut schedules = self.schedules.write().await;
            schedules.insert(
                (tenant_id, workflow_id),
                ScheduleEntry {
                    _cron_expr: cron_expr.clone(),
                    schedule,
                },
            );

            info!(
                tenant = %tenant_id,
                workflow = %workflow_id,
                cron = %cron_expr,
                "registered workflow schedule"
            );
        }
        Ok(())
    }

    pub async fn unregister(&self, tenant_id: TenantId, workflow_id: WorkflowId) {
        let mut schedules = self.schedules.write().await;
        schedules.remove(&(tenant_id, workflow_id));
    }

    /// Returns workflow IDs that are due for execution right now.
    pub async fn get_due_workflows(&self) -> Vec<(TenantId, WorkflowId)> {
        let schedules = self.schedules.read().await;
        let now = Utc::now();
        let mut due = Vec::new();

        for ((tenant_id, workflow_id), entry) in schedules.iter() {
            if let Some(next) = entry.schedule.upcoming(Utc).next() {
                let diff: chrono::TimeDelta = next - now;
                if diff.num_seconds() <= 60 {
                    due.push((*tenant_id, *workflow_id));
                }
            }
        }

        due
    }

    /// Runs the scheduler loop, checking for due workflows every 30 seconds.
    pub async fn run_loop<F>(self: Arc<Self>, mut on_due: F)
    where
        F: FnMut(TenantId, WorkflowId) + Send + 'static,
    {
        info!("workflow scheduler started");
        loop {
            let due = self.get_due_workflows().await;
            for (tenant_id, workflow_id) in due {
                info!(tenant = %tenant_id, workflow = %workflow_id, "triggering scheduled workflow");
                on_due(tenant_id, workflow_id);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        }
    }
}

impl Default for WorkflowScheduler {
    fn default() -> Self {
        Self::new()
    }
}
