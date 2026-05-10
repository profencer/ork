//! Hook surface for [`CodeAgent`](crate::code_agent::CodeAgent)
//! (ADR [`0052`](../../docs/adrs/0052-code-first-agent-dsl.md) §`Hooks`).
//!
//! `ToolHook` and `CompletionHook` are the ork-shape equivalents of rig's
//! [`PromptHook`](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html)
//! / [`ToolCallHookAction`](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/enum.ToolCallHookAction.html).
//! The current implementation fires hooks directly from
//! [`crate::rig_engine::OrkToolDyn`] rather than going through rig's hook trait
//! — semantically identical, simpler to test, and insulates ork from rig API
//! drift. Captured under the ADR's `Reviewer findings` if/when revisited.

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::llm::ToolDescriptor;
use serde_json::Value;

/// ADR-0054 §`Hook surface extensions`: the richer post-run hook
/// trait now lives in `ork-core` so the [`Agent`] port can declare
/// `inject_run_complete_hook(...)` without a circular crate
/// dependency. Re-exported here for source-compat with consumers
/// importing `ork_agents::hooks::RunCompleteHook`.
pub use ork_core::ports::scorer::RunCompleteHook;

/// Decision returned by [`ToolHook::before`].
#[derive(Debug, Clone, PartialEq)]
pub enum ToolHookAction {
    /// Invoke the tool with the original arguments.
    Proceed,
    /// Skip the tool invocation; return the supplied value to the LLM as the result.
    Override(Value),
    /// Abort the entire run. The fatal-error path in
    /// [`crate::rig_engine`] propagates this as `OrkError::Workflow("hook cancelled")`.
    Cancel,
}

/// Lifecycle hook for individual tool calls. Use cases: redaction of tool args /
/// results, policy gates (`Cancel`), audit logging (`after`), test stubs
/// (`Override`).
///
/// **Chain semantics:**
/// - `before` fires in registration order. The first non-`Proceed` decision
///   short-circuits the chain: subsequent hooks' `before` is skipped.
/// - `after` fires for **every** registered hook regardless of whether its
///   `before` ran. This keeps audit and observability hooks reliably notified
///   of every invocation outcome (real, overridden, or cancelled), even when a
///   policy hook earlier in the chain skipped them. If a hook's `after` must
///   correlate with its own `before`, gate inside the hook on its own state.
#[async_trait]
pub trait ToolHook: Send + Sync {
    async fn before(
        &self,
        ctx: &AgentContext,
        descriptor: &ToolDescriptor,
        args: &Value,
    ) -> ToolHookAction;

    async fn after(
        &self,
        ctx: &AgentContext,
        descriptor: &ToolDescriptor,
        result: &Result<Value, OrkError>,
    );
}

/// Fires once when the agent emits its terminal text. Use for span finalisation,
/// post-run scoring (ADR-0054), or audit. Hooks are called in registration order.
#[async_trait]
pub trait CompletionHook: Send + Sync {
    async fn on_completion(&self, ctx: &AgentContext, final_text: &str);
}
