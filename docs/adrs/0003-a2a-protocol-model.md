# 0003 — Adopt the A2A 1.0 protocol and message model

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 1
- **Relates to:** 0002, 0004, 0007, 0008, 0009, 0016

## Context

ork's run model — [`WorkflowRun`](../../crates/ork-core/src/models/workflow.rs) with status including A2A-aligned `input_required`, `auth_required`, and `rejected` (plus the original lifecycle values) and a list of `step_results` — backs the internal DAG. The user-facing surface is HTTP routes that return whole runs as JSON ([`crates/ork-api/src/routes/workflows.rs`](../../crates/ork-api/src/routes/workflows.rs)).

To interoperate with other agent frameworks (and to build SAM-equivalent peer delegation, `0006`), ork needs a wire-protocol vocabulary that:

- Distinguishes "task" from "message" (today they are conflated as a `WorkflowRun` with an `input` JSON value);
- Carries multimodal content (text + structured data + files), not just one prompt string;
- Models lifecycle states richer than today's enum (e.g. `input_required` for human-in-the-loop, `auth_required` for OAuth handshakes mid-task);
- Has a JSON-RPC envelope so an external client can call ork the same way it calls any other A2A agent.

The Google [Agent2Agent (A2A) 1.0 protocol](https://github.com/google/a2a) gives us exactly this. SAM uses the [`a2a-sdk` Python types](https://pypi.org/project/a2a-sdk/) directly via [`src/solace_agent_mesh/common/a2a/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/a2a). We need the Rust equivalent.

There is no first-party Rust A2A crate today, so this ADR also commits us to maintaining the types ourselves until one exists.

## Decision

ork **adopts** the A2A 1.0 type model and lands its types in a new dedicated crate **`ork-a2a`** (workspace member alongside `ork-core`), implemented under [`crates/ork-a2a/`](../../crates/ork-a2a/). Putting them in their own crate keeps `ork-core` from depending on JSON-RPC framing and lets external Rust crates depend on `ork-a2a` without dragging in the workflow engine.

`crates/ork-a2a/src/types.rs` defines (normative sketch; see crate source for the full set including `TaskEvent` for streaming):

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub url: Option<Url>,
    pub provider: Option<AgentProvider>,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    pub security_schemes: Option<HashMap<String, SecurityScheme>>,
    pub security: Option<Vec<HashMap<String, Vec<String>>>>,
    pub extensions: Option<Vec<AgentExtension>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    pub state_transition_history: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub examples: Vec<String>,
    pub input_modes: Option<Vec<String>>,
    pub output_modes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    Text { text: String, metadata: Option<JsonObject> },
    Data { data: serde_json::Value, metadata: Option<JsonObject> },
    File { file: FileRef, metadata: Option<JsonObject> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileRef {
    Bytes { name: Option<String>, mime_type: Option<String>, bytes: Base64String },
    Uri { name: Option<String>, mime_type: Option<String>, uri: Url },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub message_id: MessageId,
    pub task_id: Option<TaskId>,
    pub context_id: Option<ContextId>,
    pub metadata: Option<JsonObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub context_id: ContextId,
    pub status: TaskStatus,
    pub history: Vec<Message>,
    pub artifacts: Vec<Artifact>,
    pub metadata: Option<JsonObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}
```

`crates/ork-a2a/src/jsonrpc.rs` defines envelope types: `JsonRpcRequest<P>`, `JsonRpcResponse<R>`, `JsonRpcError`, and the method enum:

```rust
pub enum A2aMethod {
    MessageSend,                       // "message/send"
    MessageStream,                     // "message/stream"
    TasksGet,                          // "tasks/get"
    TasksCancel,                       // "tasks/cancel"
    TasksPushNotificationConfigSet,    // "tasks/pushNotificationConfig/set"
    TasksPushNotificationConfigGet,    // "tasks/pushNotificationConfig/get"
}
```

ork-internal types ([`AgentMessage`, `AgentEvent`, `AgentContext`] introduced in ADR [`0002`](0002-agent-port.md)) are **type aliases** for the A2A types — no extra layer:

```rust
pub type AgentMessage = ork_a2a::Message;
pub type AgentCard = ork_a2a::AgentCard;
pub type AgentEvent = ork_a2a::TaskEvent; // status_update | artifact_update | message
```

### Mapping to existing ork models

[`WorkflowRun`](../../crates/ork-core/src/models/workflow.rs) is **not** replaced by `Task` — instead `Task` is materialised on demand by joining `WorkflowRun` with a new `a2a_tasks` table introduced in ADR [`0008`](0008-a2a-server-endpoints.md). Mapping:

| A2A `Task` field | ork source |
| ---------------- | ---------- |
| `id` | new `a2a_tasks.id` UUID, distinct from `WorkflowRun.id` |
| `context_id` | new column; correlates multi-task conversations |
| `status.state` | derived from [`WorkflowRunStatus`](../../crates/ork-core/src/models/workflow.rs) (see table below) |
| `history` | reconstructed from a new `a2a_messages` table |
| `artifacts` | from the artifact store in ADR [`0016`](0016-artifact-storage.md) |

`WorkflowRunStatus` is **extended** with two variants required by the A2A spec but missing today:

```rust
pub enum WorkflowRunStatus {
    Pending,        // ↔ TaskState::Submitted
    Running,        // ↔ TaskState::Working
    InputRequired,  // ↔ TaskState::InputRequired   (NEW)
    AuthRequired,   // ↔ TaskState::AuthRequired    (NEW)
    Completed,      // ↔ TaskState::Completed
    Failed,         // ↔ TaskState::Failed
    Cancelled,      // ↔ TaskState::Canceled
    Rejected,       // ↔ TaskState::Rejected        (NEW)
}
```

### Versioning

The `ork-a2a` crate's semver tracks the A2A protocol version. The wire format is fixed at A2A 1.0; minor protocol revisions land as additive fields with `serde(default)` to preserve backwards compatibility.

## Consequences

### Positive

- A2A clients (browser, CLI, vendor agents) talk to ork using the standard wire format with no translation layer.
- One Rust type set serves both the inbound A2A server (ADR [`0008`](0008-a2a-server-endpoints.md)) and the outbound `A2aRemoteAgent` client (ADR [`0007`](0007-remote-a2a-agent-client.md)).
- The `Part` enum unblocks file/multimodal exchange, which is the basis for the artifact pipeline in [`0016`](0016-artifact-storage.md).
- Existing JSON-as-prompt-string flow becomes a degenerate single-`TextPart` case; no breaking change to current workflows.

### Negative / costs

- We own a new crate that mirrors the A2A spec. If/when an upstream `a2a-rs` becomes credible, we may switch and own a thin shim instead.
- Database schema gains two new tables (`a2a_tasks`, `a2a_messages`) plus three new `WorkflowRunStatus` variants — both require migrations.
- `WorkflowRunStatus` becoming non-exhaustive over time will require pattern-match audits.

### Neutral / follow-ups

- ADR [`0008`](0008-a2a-server-endpoints.md) defines the migration adding the new tables.
- ADR [`0009`](0009-push-notifications.md) consumes `pushNotificationConfig` types from this crate.
- Streaming envelope (`SendStreamingMessageResponse` + SSE chunk format) is fully specified in ADR [`0008`](0008-a2a-server-endpoints.md).

## Alternatives considered

- **Embed A2A types inside `ork-core`.** Rejected: forces `ork-core` to depend on JSON-RPC framing and prevents external crates (gateways, clients) from importing them without a heavy dependency.
- **Wait for a community Rust crate.** Rejected: there is no production-grade `a2a-rs` today; this ADR cannot block on one.
- **Define a custom ork wire format and translate at the edge.** Rejected: pointless impedance mismatch, kills the parity goal, and forces every downstream consumer to learn two vocabularies.
- **Use `serde_json::Value` everywhere instead of typed structs.** Rejected: typed structs catch wire-format breakage at compile time, which is the whole point of using Rust.

## Affected ork modules

- New crate: `crates/ork-a2a/` (added to root [`Cargo.toml`](../../Cargo.toml) workspace members).
- [`crates/ork-core/src/ports/agent.rs`](../../crates/ork-core/src/ports/) — type aliases, no concrete types of its own.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) — `WorkflowRunStatus` extended.
- [`crates/ork-persistence/src/postgres/workflow_repo.rs`](../../crates/ork-persistence/src/postgres/workflow_repo.rs) — handle new statuses; new repo for `a2a_tasks` lands in ADR [`0008`](0008-a2a-server-endpoints.md).
- New SQL migration `migrations/002_a2a_tasks.sql` (defined in ADR [`0008`](0008-a2a-server-endpoints.md)).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `a2a-sdk` types (`AgentCard`, `Task`, `Message`, `Part`) | [`src/solace_agent_mesh/common/a2a/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/a2a) (re-exported from `a2a.types`) | `ork-a2a::types` |
| Message helpers `TextPart` / `DataPart` / `FilePart` | [`common/a2a/message.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/message.py) | `Part::{Text, Data, File}` |
| `TaskState` lifecycle | [`common/a2a/task.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/task.py) | `TaskState` enum + extended `WorkflowRunStatus` |
| Topic protocol helpers | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) | `crates/ork-a2a/src/topics.rs` (Kafka topic names per ADR [`0004`](0004-hybrid-kong-kafka-transport.md)) |

## Open questions

- Should `Part::File` keep both the `Bytes` and `Uri` variants in transit, or canonicalise to `Uri` once the artifact store (ADR [`0016`](0016-artifact-storage.md)) is wired up? Initially: keep both; the artifact store may rewrite inbound `Bytes` parts to `Uri` parts before persistence.
- Should `AgentExtension` URIs be typed (e.g. an `enum`)? Initially: stringly-typed; ADR [`0005`](0005-agent-card-and-devportal-discovery.md) defines the ork-specific extension URIs.

## References

- A2A 1.0 spec: <https://github.com/google/a2a>
- SAM `common/a2a/protocol.py`: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py>
- [`future-a2a.md` §2–§3](../../future-a2a.md)
