# 0002 — Introduce an `Agent` port in `ork-core`

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 1
- **Relates to:** 0003, 0006, 0007, 0008, 0011, 0018

## Context

ork's workflow engine previously called the LLM provider directly. This ADR introduces the `Agent` port so execution goes through [`Agent::send_stream`](../../crates/ork-core/src/ports/agent.rs); [`WorkflowEngine`](../../crates/ork-core/src/workflow/engine.rs) resolves `node.agent` via [`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs) and [`LocalAgent`](../../crates/ork-agents/src/local.rs) wraps the LLM plus [`AgentConfig`](../../crates/ork-core/src/models/agent.rs) (temperature, max_tokens, system prompt, config-scoped tools).

Concretely the gaps are:

- The `agent:` field on a [`WorkflowStep`](../../crates/ork-core/src/models/workflow.rs) is an open `String`, but the engine resolves it via a hardcoded `match` on role names. Any non-matching name silently falls through to a default system prompt.
- Tools listed in `AgentConfig.tools` are never read by the engine; only the per-step `tools:` list is.
- There is no way to register "this agent lives over A2A on `https://vendor.example.com`" — there is no contract for what an agent is in the first place.
- Streaming, cancellation, and tool calls are bolted onto `LlmProvider::chat_stream`, not onto an "agent" surface.

SAM's equivalent is [`SamAgentComponent`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/sac/component.py): a single uniform agent runtime that composes ADK tools, peer tools, artifact services, and an `AgentCard`. Both local and proxied A2A agents satisfy the same shape.

This ADR introduces the analogous Rust trait. It is the **load-bearing decision** for everything else in this set: A2A endpoints, peer delegation, MCP, gateways, embeds, and remote agents all bind to this trait.

## Decision

ork **introduces** a new `Agent` port, sibling to the existing `LlmProvider`, `WorkflowRepository`, `IntegrationAdapter`, and `Workspace` ports under [`crates/ork-core/src/ports/`](../../crates/ork-core/src/ports/).

```rust
// crates/ork-core/src/ports/agent.rs
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    fn id(&self) -> &AgentId;
    fn card(&self) -> &AgentCard;

    async fn send(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentMessage, OrkError>;

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<BoxStream<'static, Result<AgentEvent, OrkError>>, OrkError>;

    async fn cancel(
        &self,
        ctx: AgentContext,
        task_id: &TaskId,
    ) -> Result<(), OrkError> { Err(OrkError::Unsupported("cancel".into())) }
}
```

`AgentCard`, `AgentMessage`, `AgentEvent`, `AgentContext`, `TaskId` are the A2A-aligned types defined in ADR [`0003`](0003-a2a-protocol-model.md).

`AgentContext` carries the request envelope: `tenant_id: TenantId`, `task_id: TaskId`, `parent_task_id: Option<TaskId>`, `cancel: CancellationToken`, `caller: CallerIdentity`, optional `push_notification_url: Option<Url>`, and a `trace_ctx` (W3C traceparent).

The former four-role enum (`AgentRole`) is **removed** in favour of an open `AgentId` (`String` alias in [`a2a/context.rs`](../../crates/ork-core/src/a2a/context.rs)). Existing role names continue to resolve as the IDs `"planner"`, `"researcher"`, `"writer"`, `"reviewer"`, plus `"synthesizer"` for [`change-plan.yaml`](../../workflow-templates/change-plan.yaml), so workflow YAMLs in [`workflow-templates/`](../../workflow-templates/) keep working unchanged.

A `LocalAgent` struct implements `Agent` by wrapping today's logic:

```rust
pub struct LocalAgent {
    id: AgentId,
    card: AgentCard,
    config: AgentConfig,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolExecutor>,
}
```

[`AgentRegistry`](../../crates/ork-agents/src/registry.rs) becomes a registry of `Arc<dyn Agent>` keyed by `AgentId`, with helpers `list_cards() -> Vec<AgentCard>` and `resolve(&AgentId) -> Option<Arc<dyn Agent>>`.

[`WorkflowEngine::execute_agent_step`](../../crates/ork-core/src/workflow/engine.rs) is rewritten to:

1. Resolve `node.agent` (`String`) into an `Arc<dyn Agent>` via the registry.
2. Build an `AgentMessage` from the resolved prompt template (single `TextPart` initially; later ADRs add `DataPart` and `FilePart`).
3. Call `agent.send_stream(ctx, msg)` and forward `AgentEvent`s to the streaming channel introduced in ADR [`0010`](0010-mcp-tool-plane.md) and [`0022`](0022-observability.md).
4. Materialise the final `AgentMessage` back into a [`StepResult`](../../crates/ork-core/src/models/workflow.rs).

The hardcoded `temperature: 0.3, max_tokens: 4096` in [`engine.rs`](../../crates/ork-core/src/workflow/engine.rs) move into `AgentConfig` (already present but unused) and become per-agent.

`AppState.agent_registry` (declared in [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) but not currently used by the engine) is finally read by `WorkflowEngine`.

## Consequences

### Positive

- `WorkflowEngine` no longer knows whether an agent is local or remote — that distinction moves into the trait impl, which is the precondition for [`0007`](0007-remote-a2a-agent-client.md), [`0008`](0008-a2a-server-endpoints.md), and the peer delegation in [`0006`](0006-peer-delegation.md).
- The mismatch between `AgentConfig` (defined in [`AgentConfig`](../../crates/ork-core/src/models/agent.rs)) and what the engine actually uses goes away.
- A single `AgentCard` is the source of truth for both the A2A `/.well-known/agent-card.json` endpoint and the runtime registry's `list_cards()`.
- Streaming, cancellation, and (later) push notifications all live behind one surface.

### Negative / costs

- Workflow execution gains one indirection (registry resolve + dyn dispatch). At today's per-step latencies this is negligible but worth noting.
- Downstream code that depended on the `AgentRole` enum must use string IDs or resolve via [`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs).
- The `ork-agents` crate's purpose narrows; eventually it may merge into `ork-core`.

### Neutral / follow-ups

- A separate ADR ([`0011`](0011-native-llm-tool-calling.md)) refactors how tools are executed inside `LocalAgent`; this ADR keeps the existing append-tool-output-to-prompt behaviour intact to keep the diff small.
- `cancel()` ships with a default `Unsupported` impl. ADR [`0008`](0008-a2a-server-endpoints.md) requires it for `tasks/cancel` handling and ADR [`0018`](0018-dag-executor-enhancements.md) wires it through the engine.

## Alternatives considered

- **Generic over `A: Agent` instead of `dyn Agent`.** Rejected: the registry needs to hold heterogeneous agent types in one map (some `LocalAgent`, some `A2aRemoteAgent`, later some plugin-provided types), so dynamic dispatch is required.
- **Keep `AgentRole` enum and add a separate `RemoteAgent` table.** Rejected: forces the engine to branch on every "is this local or remote" decision and prevents plugins from contributing new agent types.
- **Make `LlmProvider` the agent trait.** Rejected: an LLM is not an agent; an agent owns an LLM, tools, an artifact service, and a card. Conflating them blocks remote A2A agents that have no LLM at all.

## Affected ork modules

- New: `crates/ork-core/src/ports/agent.rs`, `crates/ork-core/src/a2a/` (types), [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs) re-exports.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — `execute_agent_step` rewritten.
- [`crates/ork-core/src/models/agent.rs`](../../crates/ork-core/src/models/agent.rs) — `AgentRole` removed; `AgentConfig` is `LocalAgent` seed config (`id`, `name`, `description`, prompts, tools, LLM params).
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — `build_default_registry` seeds [`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs) with `LocalAgent` instances.
- [`crates/ork-agents/src/roles.rs`](../../crates/ork-agents/src/roles.rs) — promoted to seed values for `LocalAgent` instances.
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) — `agent_registry` actually wired into `engine`.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `SamAgentComponent` | [`src/solace_agent_mesh/agent/sac/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/sac/component.py) | `LocalAgent` impl of `Agent` |
| Agent registry (TTL cache) | [`src/solace_agent_mesh/common/agent_registry.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/agent_registry.py) | New `AgentRegistry` over `Arc<dyn Agent>` |
| Agent card | [`src/solace_agent_mesh/common/a2a/utils.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/utils.py) | `AgentCard` returned by `Agent::card` |
| Per-agent ADK tool list | YAML `tools:` in [`templates/agent_template.yaml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/templates/agent_template.yaml) | `AgentConfig.tools` actually consumed inside `LocalAgent::send` |

## Open questions

- Should `AgentCard` be an associated type or a runtime field? Decision: runtime field, because `A2aRemoteAgent` discovers its card lazily from `/.well-known/agent-card.json`.
- Should `AgentMessage` be re-exported from `ork-core` or live in a dedicated `ork-a2a` crate? Decided in ADR [`0003`](0003-a2a-protocol-model.md).

## References

- [`future-a2a.md` §2](../../future-a2a.md) — "Add an `Agent` port in `ork-core`"
- A2A spec: <https://github.com/google/a2a>
- SAM source: <https://github.com/SolaceLabs/solace-agent-mesh>
