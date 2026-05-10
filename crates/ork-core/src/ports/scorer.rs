//! Scorer port and value types
//! ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).
//!
//! Concrete scorers (deterministic, LLM-as-judge), the live-sampling
//! worker, and the offline `OrkEval` runner live in `ork-eval`. This
//! module owns only the trait and the data shapes the trait talks in
//! so domain code in `ork-core` and `ork-agents` can attach hooks
//! without depending on the heavier `ork-eval` surface.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::a2a::AgentContext;

/// Stable id for a scored run (agent or workflow).
///
/// New runs get a fresh `RunId`; the offline runner mints one per
/// dataset example so each scored row in `scorer_results` traces back
/// to a single replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the scored run was an agent or a workflow execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Agent,
    Workflow,
}

/// One tool invocation observed during a scored run.
///
/// Populated by the agent runtime's tool-call hook (and the workflow
/// engine's per-step capture). `args` and `result` are always JSON;
/// `error` is `Some` when the call failed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: Value,
    #[serde(default)]
    pub result: Value,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Execution trace assembled across a single agent / workflow run.
///
/// The shape is deliberately small: scorers consume tool-call recall,
/// per-call latency, and bracketing timestamps; richer telemetry
/// (per-step LLM output, retries) is captured elsewhere and not
/// duplicated here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub user_message: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

impl Trace {
    #[must_use]
    pub fn duration(&self) -> Duration {
        let delta_ms = (self.completed_at - self.started_at)
            .num_milliseconds()
            .max(0) as u64;
        Duration::from_millis(delta_ms)
    }
}

/// Borrowed input handed to [`Scorer::score`].
///
/// The lifetime is tied to the live-sampling worker / offline runner's
/// per-run scope so heavy buffers (`final_response`, `trace`) are not
/// cloned once per attached scorer.
pub struct ScoreInput<'a> {
    pub run_id: RunId,
    pub run_kind: RunKind,
    pub agent_id: Option<&'a str>,
    pub workflow_id: Option<&'a str>,
    pub user_message: &'a str,
    pub final_response: &'a str,
    pub trace: &'a Trace,
    pub expected: Option<&'a Value>,
    pub context: &'a AgentContext,
}

/// Scorer output. `score ∈ [0.0, 1.0]`; `details` carries scorer-specific
/// extras (judge tokens, failed tool-call expectations, etc.).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoreCard {
    pub score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default)]
    pub details: Value,
}

/// Self-description used by Studio (ADR-0055) and the offline report
/// to render scorer rows without re-deriving labels per row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoreSchema {
    pub id: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_set: Option<Vec<String>>,
    #[serde(default)]
    pub details: Value,
}

#[async_trait]
pub trait Scorer: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ScoreSchema;

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError>;
}

/// Richer post-run hook fired by the agent runtime
/// ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
/// §`Hook surface extensions`). Lives in `ork-core` so the
/// [`crate::ports::agent::Agent`] trait can declare an injection
/// method without depending on `ork-agents` (which is downstream of
/// `ork-core`). The concrete agent runtime in `ork-agents` re-exports
/// this trait under `ork_agents::hooks::RunCompleteHook`.
///
/// **Fire ordering** (rig engine):
/// - Success path: every `CompletionHook` fires first, then this hook
///   fires with `error = None`.
/// - Cancel / fatal / `tool_loop_exceeded`: only this hook fires,
///   with `error = Some(&e)` and `final_text = ""`.
#[async_trait]
pub trait RunCompleteHook: Send + Sync {
    async fn on_run_complete(
        &self,
        ctx: &AgentContext,
        user_message: &str,
        final_text: &str,
        trace: &Trace,
        error: Option<&OrkError>,
    );
}

/// Mutable accumulator the agent / workflow runtime fills during a
/// run. The runtime appends a [`ToolCallRecord`] per tool invocation
/// and snapshots the assembled [`Trace`] at completion via
/// [`TraceCaptureHandle::snapshot`].
///
/// Lives in `ork-core` so the agent runtime (`ork-agents`) can
/// instantiate one without depending on `ork-eval`.
pub struct TraceCapture {
    started_wall: DateTime<Utc>,
    started_mono: Instant,
    user_message: String,
    tool_calls: Mutex<Vec<ToolCallRecord>>,
}

impl TraceCapture {
    #[must_use]
    pub fn start(user_message: impl Into<String>) -> Self {
        Self {
            started_wall: Utc::now(),
            started_mono: Instant::now(),
            user_message: user_message.into(),
            tool_calls: Mutex::new(Vec::new()),
        }
    }

    pub fn record_tool_call(
        &self,
        name: impl Into<String>,
        args: Value,
        result: Value,
        duration_ms: u64,
        error: Option<String>,
    ) {
        let record = ToolCallRecord {
            name: name.into(),
            args,
            result,
            duration_ms,
            error,
        };
        if let Ok(mut guard) = self.tool_calls.lock() {
            guard.push(record);
        }
    }

    /// Wall-clock duration since [`Self::start`] in milliseconds.
    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.started_mono.elapsed().as_millis() as u64
    }

    /// Snapshot the trace by cloning the recorded calls. Safe while
    /// other clones of the wrapping handle are still alive.
    #[must_use]
    pub fn snapshot(&self) -> Trace {
        let tool_calls = self
            .tool_calls
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        Trace {
            user_message: self.user_message.clone(),
            tool_calls,
            started_at: self.started_wall,
            completed_at: Utc::now(),
        }
    }
}

/// Cheap, cloneable handle to a [`TraceCapture`] for components that
/// only need to append records (e.g. the agent runtime's tool
/// dispatch path).
#[derive(Clone)]
pub struct TraceCaptureHandle {
    inner: Arc<TraceCapture>,
}

impl TraceCaptureHandle {
    #[must_use]
    pub fn new(capture: TraceCapture) -> Self {
        Self {
            inner: Arc::new(capture),
        }
    }

    pub fn record_tool_call(
        &self,
        name: impl Into<String>,
        args: Value,
        result: Value,
        duration_ms: u64,
        error: Option<String>,
    ) {
        self.inner
            .record_tool_call(name, args, result, duration_ms, error);
    }

    #[must_use]
    pub fn snapshot(&self) -> Trace {
        self.inner.snapshot()
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.inner.elapsed_ms()
    }
}

#[cfg(test)]
mod trace_capture_tests {
    use super::*;

    #[test]
    fn capture_records_and_snapshots() {
        let cap = TraceCaptureHandle::new(TraceCapture::start("hello"));
        cap.record_tool_call(
            "weather",
            serde_json::json!({"city": "SF"}),
            serde_json::json!({"high_f": 70}),
            12,
            None,
        );
        let trace = cap.snapshot();
        assert_eq!(trace.user_message, "hello");
        assert_eq!(trace.tool_calls.len(), 1);
        assert_eq!(trace.tool_calls[0].name, "weather");
        assert!(trace.completed_at >= trace.started_at);
    }

    #[test]
    fn snapshot_does_not_drain() {
        let cap = TraceCaptureHandle::new(TraceCapture::start(""));
        cap.record_tool_call("a", Value::Null, Value::Null, 1, None);
        let _ = cap.snapshot();
        // capture still has the record
        let again = cap.snapshot();
        assert_eq!(again.tool_calls.len(), 1);
    }
}
