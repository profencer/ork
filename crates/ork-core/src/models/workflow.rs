use chrono::{DateTime, Utc};
use ork_a2a::{TaskId, TaskState};
use ork_common::config::A2aAuthToml;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub id: WorkflowId,
    pub tenant_id: TenantId,
    pub name: String,
    pub version: String,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStep>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    Schedule { cron: String },
    Webhook { event: String },
    Manual,
}

/// How a [`WorkflowStep`] points at the agent that should run it (ADR-0007 §3
/// `Workflow-time inline card`).
///
/// - [`Self::Id`] — the bare-string form already supported in YAML/JSON. Resolved
///   against the global [`crate::agent_registry::AgentRegistry`] (local agents,
///   plus remote agents materialised by the static loader or discovery).
/// - [`Self::Inline`] — author embeds a card URL + auth in-line. The engine
///   builds a transient `A2aRemoteAgent` for the lifetime of the run, stored in a
///   per-run overlay so the global registry is never mutated.
///
/// `#[serde(untagged)]` keeps the existing bare-string YAML round-tripping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum WorkflowAgentRef {
    Id(String),
    Inline {
        /// Card URL on the remote — typically `<base>/.well-known/agent-card.json`
        /// but any URL the engine can `GET` works (the base is recovered by
        /// stripping the path).
        url: Url,
        #[serde(default)]
        auth: Option<A2aAuthToml>,
    },
}

impl WorkflowAgentRef {
    /// Best-effort id used in logs/metrics. For [`Self::Inline`] we synthesize
    /// `inline:<host>` so the audit trail is meaningful before the engine
    /// has built the transient agent.
    pub fn display_id(&self) -> String {
        match self {
            Self::Id(id) => id.clone(),
            Self::Inline { url, .. } => format!("inline:{}", url.host_str().unwrap_or("?")),
        }
    }

    /// True when this ref does NOT need a per-run overlay — the registry alone
    /// can resolve it.
    pub fn is_id(&self) -> bool {
        matches!(self, Self::Id(_))
    }
}

/// `"some-id".into()` ergonomics for the test corpus + any upstream code that
/// still works with bare agent ids. Inline cards must be constructed
/// explicitly because they require a URL and can't be inferred from a string.
impl From<&str> for WorkflowAgentRef {
    fn from(id: &str) -> Self {
        Self::Id(id.to_string())
    }
}

impl From<String> for WorkflowAgentRef {
    fn from(id: String) -> Self {
        Self::Id(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub agent: WorkflowAgentRef,
    /// LLM-facing tool allow-list for this step.
    ///
    /// ADR 0011 changes this field from the legacy "pre-execute these tools in
    /// order before the LLM call" meaning to "these tools may be exposed to the
    /// model, alongside always-available builtins such as `agent_call`". Existing
    /// workflow YAML keeps the same `tools:` shape; `ork workflow migrate-tools`
    /// can prepend the old prompt hint for workflows that relied on eager tool
    /// execution.
    pub tools: Vec<String>,
    pub prompt_template: String,
    /// Optional per-step provider override (ADR 0012 §`Selection`). Highest
    /// precedence in the resolution chain (step → agent → tenant default →
    /// operator default); `None` falls through to [`crate::models::agent::AgentConfig::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional per-step model override (ADR 0012 §`Selection`). Resolved
    /// after [`Self::provider`]; `None` falls through to [`crate::models::agent::AgentConfig::model`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub condition: Option<StepCondition>,
    /// Template resolving to a JSON array; step runs once per element (see workflow engine).
    #[serde(default)]
    pub for_each: Option<String>,
    /// Variable name for the current element in templates (default `item`).
    #[serde(default)]
    pub iteration_var: Option<String>,
    /// Optional peer-delegation hop after the parent step's output is produced
    /// (ADR 0006 §`b) delegate workflow step`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_to: Option<DelegationSpec>,
}

/// Per ADR 0006 §`b) delegate workflow step`. Authored in YAML as the `delegate_to:`
/// block on a [`WorkflowStep`] and propagated unchanged to the compiled
/// [`crate::workflow::compiler::WorkflowNode`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationSpec {
    /// Target agent id (resolved through [`crate::agent_registry::AgentRegistry`]; may be local or remote).
    pub agent: String,
    /// Prompt template for the child task; same syntax as [`WorkflowStep::prompt_template`].
    /// Required when [`Self::child_workflow`] is `None`.
    #[serde(default)]
    pub prompt_template: String,
    /// Whether the parent step blocks on the child task. Defaults to `true` per the ADR.
    /// Renamed to `await_` in Rust to dodge the `await` reserved keyword.
    #[serde(default = "default_true", rename = "await")]
    pub await_: bool,
    /// Optional push-notification URL for fire-and-forget (`await: false`) results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_url: Option<url::Url>,
    /// Workflow id to invoke instead of a single send. Forks a child [`WorkflowRun`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_workflow: Option<WorkflowId>,
    /// Per-call timeout; falls back to the engine default when `None`.
    /// In YAML/JSON: integer seconds (`60`) or a duration string (`"60s"`, `"5m"`).
    #[serde(
        default,
        with = "duration_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
}

fn default_true() -> bool {
    true
}

/// serde helper for `Option<Duration>` accepting either integer seconds or a
/// duration string (`"60s"`, `"5m"`, `"1h"`). Keeps the YAML readable without
/// pulling in a new workspace dep.
mod duration_str {
    use super::Duration;
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(value: &Option<Duration>, ser: S) -> Result<S::Ok, S::Error> {
        match value {
            None => ser.serialize_none(),
            Some(d) => ser.serialize_str(&format!("{}s", d.as_secs())),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Duration>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            None,
            Secs(u64),
            Str(String),
        }
        match Option::<Repr>::deserialize(de)? {
            None | Some(Repr::None) => Ok(None),
            Some(Repr::Secs(s)) => Ok(Some(Duration::from_secs(s))),
            Some(Repr::Str(s)) => parse(&s).map(Some).map_err(de::Error::custom),
        }
    }

    fn parse(input: &str) -> Result<Duration, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("empty duration string".into());
        }
        let (num_part, unit) = s
            .find(|c: char| !c.is_ascii_digit())
            .map_or((s, "s"), |idx| (&s[..idx], s[idx..].trim()));
        let n: u64 = num_part
            .parse()
            .map_err(|e| format!("invalid duration number `{num_part}`: {e}"))?;
        match unit {
            "" | "s" | "sec" | "secs" => Ok(Duration::from_secs(n)),
            "ms" => Ok(Duration::from_millis(n)),
            "m" | "min" | "mins" => Ok(Duration::from_secs(n * 60)),
            "h" | "hr" | "hrs" => Ok(Duration::from_secs(n * 3600)),
            other => Err(format!("unknown duration unit `{other}` (use s, ms, m, h)")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepCondition {
    pub on_pass: String,
    pub on_fail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    pub workflow_id: WorkflowId,
    pub tenant_id: TenantId,
    pub status: WorkflowRunStatus,
    pub input: serde_json::Value,
    pub output: Option<serde_json::Value>,
    pub step_results: Vec<StepResult>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Parent run id when this run was forked by a `delegate_to: { child_workflow }` step
    /// (ADR 0006). `None` for a top-level run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<WorkflowRunId>,
    /// Parent step id (the step that triggered the delegation). `None` for a top-level run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_step_id: Option<String>,
    /// A2A task id of the parent task (ADR 0008 `a2a_tasks.parent_task_id`). `None` for
    /// a top-level run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Pending,
    Running,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Cancelled,
    Rejected,
}

impl std::fmt::Display for WorkflowRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::InputRequired => write!(f, "input_required"),
            Self::AuthRequired => write!(f, "auth_required"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// Maps workflow run status to A2A [`TaskState`] (ADR 0003).
impl From<WorkflowRunStatus> for TaskState {
    fn from(s: WorkflowRunStatus) -> Self {
        match s {
            WorkflowRunStatus::Pending => Self::Submitted,
            WorkflowRunStatus::Running => Self::Working,
            WorkflowRunStatus::InputRequired => Self::InputRequired,
            WorkflowRunStatus::AuthRequired => Self::AuthRequired,
            WorkflowRunStatus::Completed => Self::Completed,
            WorkflowRunStatus::Failed => Self::Failed,
            WorkflowRunStatus::Cancelled => Self::Canceled,
            WorkflowRunStatus::Rejected => Self::Rejected,
        }
    }
}

/// Maps A2A [`TaskState`] to persisted [`WorkflowRunStatus`] (spelling: `canceled` ↔ `cancelled`).
impl From<TaskState> for WorkflowRunStatus {
    fn from(s: TaskState) -> Self {
        match s {
            TaskState::Submitted => Self::Pending,
            TaskState::Working => Self::Running,
            TaskState::InputRequired => Self::InputRequired,
            TaskState::AuthRequired => Self::AuthRequired,
            TaskState::Completed => Self::Completed,
            TaskState::Failed => Self::Failed,
            TaskState::Canceled => Self::Cancelled,
            TaskState::Rejected => Self::Rejected,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_id: String,
    pub agent: String,
    pub status: StepStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed,
}
