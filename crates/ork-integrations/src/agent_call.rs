//! `agent_call` tool executor (ADR
//! [`0006`](../../../../docs/adrs/0006-peer-delegation.md)).
//!
//! The LLM-facing `agent_call` tool is the minimum-viable peer-delegation
//! surface: the model decides "ask agent X to do Y" and the runtime forks a
//! child A2A task. The actual delegation logic lives in
//! [`ork_core::workflow::delegation`] so the engine's `delegate_to` step and
//! this tool share one code path.
//!
//! Per ADR 0011 §`Engine cleanup` the per-instance "caller context seam"
//! (the `RwLock<Option<AgentContext>>` plus
//! `set_caller_context`/`clear_caller_context` defaults on the
//! [`ToolExecutor`] trait) is gone. `parent_ctx` now flows in directly via
//! the `ctx: &AgentContext` arg on [`ToolExecutor::execute`].

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use ork_a2a::AgentCallInput;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::a2a_task_repo::A2aTaskRepository;
use ork_core::ports::delegation_publisher::DelegationPublisher;
use ork_core::workflow::delegation::{execute_one_shot_delegation, map_input_err};
use ork_core::workflow::engine::ToolExecutor;

/// Tool executor for the `agent_call` tool. Routes requests through the shared
/// [`execute_one_shot_delegation`] helper so delegation semantics match the
/// `delegate_to:` workflow step.
///
/// The registry is held as a [`Weak`] reference because the registry's local
/// agents themselves own this tool executor (via the composite executor). Using
/// [`Arc`] would create a cycle that prevented graceful shutdown. The wiring
/// site uses [`Arc::new_cyclic`] to materialise the [`Weak`].
pub struct AgentCallToolExecutor {
    registry: Weak<AgentRegistry>,
    publisher: Option<Arc<dyn DelegationPublisher>>,
    a2a_tasks: Option<Arc<dyn A2aTaskRepository>>,
}

impl AgentCallToolExecutor {
    /// Build an executor that resolves delegation targets through the supplied
    /// (weak) registry handle.
    #[must_use]
    pub fn new(
        registry: Weak<AgentRegistry>,
        publisher: Option<Arc<dyn DelegationPublisher>>,
        a2a_tasks: Option<Arc<dyn A2aTaskRepository>>,
    ) -> Self {
        Self {
            registry,
            publisher,
            a2a_tasks,
        }
    }

    fn registry(&self) -> Result<Arc<AgentRegistry>, OrkError> {
        self.registry.upgrade().ok_or_else(|| {
            OrkError::Internal(
                "AgentCallToolExecutor lost its AgentRegistry handle (process shutdown?)".into(),
            )
        })
    }
}

#[async_trait]
impl ToolExecutor for AgentCallToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        if tool_name != "agent_call" {
            return Err(OrkError::Integration(format!(
                "AgentCallToolExecutor cannot handle tool '{tool_name}'"
            )));
        }

        // TODO(ADR-0021): once the central RBAC helper lands, gate this on the
        //                 `agent:<input.agent>:delegate` scope of `ctx.caller`.

        let parsed = AgentCallInput::from_value(input).map_err(map_input_err)?;
        let registry = self.registry()?;

        let outcome = execute_one_shot_delegation(
            ctx,
            &registry,
            self.publisher.as_ref(),
            self.a2a_tasks.as_ref(),
            // The `agent_call` tool is invoked from inside an agent loop; it has no
            // workflow_run_id. The engine's delegate_to path passes the run id.
            None,
            parsed,
        )
        .await?;

        Ok(outcome.to_tool_value())
    }
}
