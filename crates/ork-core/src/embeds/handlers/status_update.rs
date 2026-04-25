//! `«status_update:task-uuid | summary|state|json»` — A2A task status from the ledger.

use std::str::FromStr;

use async_trait::async_trait;
use ork_a2a::{Part, TaskId, TaskState};

use crate::ports::a2a_task_repo::A2aTaskRepository;

use super::super::{EmbedContext, EmbedError, EmbedHandler, EmbedOutput, EmbedPhase};

/// Built-in `status_update` embed (ADR-0015; phase `Both`).
pub struct StatusUpdateHandler {
    // Shared via [`EmbedContext::a2a_repo`]; this type exists for symmetry with other handlers.
    _marker: std::marker::PhantomData<()>,
}

impl StatusUpdateHandler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl Default for StatusUpdateHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbedHandler for StatusUpdateHandler {
    fn type_id(&self) -> &'static str {
        "status_update"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Both
    }

    async fn resolve(
        &self,
        expr: &str,
        format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError> {
        let Some(repo) = ctx.a2a_repo.as_ref() else {
            return Err(EmbedError::Handler(anyhow::anyhow!(
                "status_update embed requires A2A task repository"
            )));
        };
        let tid = TaskId::from_str(expr.trim())
            .map_err(|e| EmbedError::InvalidExpression(format!("task id: {e}")))?;
        let tenant = ctx.tenant_id;
        let row = repo
            .get_task(tenant, tid)
            .await
            .map_err(|e: ork_common::error::OrkError| {
                EmbedError::Handler(anyhow::anyhow!(e.to_string()))
            })?
            .ok_or_else(|| {
                EmbedError::InvalidExpression(format!("no task {tid} for status_update"))
            })?;

        let fmt = format.map(str::trim).filter(|s| !s.is_empty());
        let s = match fmt {
            None => summary_string(repo.as_ref(), tenant, tid, &row).await?,
            Some(f) => {
                let f = f.to_lowercase();
                match f.as_str() {
                    "summary" => summary_string(repo.as_ref(), tenant, tid, &row).await?,
                    "state" | "raw_state" => state_snake(&row.state),
                    "json" | "raw" => {
                        serde_json::to_string(&row).map_err(|e| EmbedError::Handler(e.into()))?
                    }
                    _ => {
                        return Err(EmbedError::InvalidFormat(f));
                    }
                }
            }
        };
        Ok(EmbedOutput::Text(s))
    }
}

/// Human-readable one-liner: state + last message text if any.
async fn summary_string(
    repo: &dyn A2aTaskRepository,
    tenant_id: ork_common::types::TenantId,
    task_id: TaskId,
    row: &crate::ports::a2a_task_repo::A2aTaskRow,
) -> Result<String, EmbedError> {
    let messages = repo
        .list_messages(tenant_id, task_id, Some(5))
        .await
        .map_err(|e: ork_common::error::OrkError| {
            EmbedError::Handler(anyhow::anyhow!(e.to_string()))
        })?;
    let state_s = state_snake(&row.state);
    let mut tail: Option<String> = None;
    for m in messages.iter().rev() {
        if let Ok(parts) = serde_json::from_value::<Vec<Part>>(m.parts.clone()) {
            let t: String = parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if !t.is_empty() {
                tail = Some(t.chars().take(200).collect::<String>().replace('\n', " "));
                break;
            }
        }
    }
    Ok(match tail {
        Some(t) => format!("{state_s} — {t}"),
        None => state_s,
    })
}

fn state_snake(s: &TaskState) -> String {
    match s {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::InputRequired => "input_required",
        TaskState::AuthRequired => "auth_required",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
    }
    .to_string()
}
