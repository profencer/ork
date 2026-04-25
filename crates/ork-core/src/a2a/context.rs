use ork_a2a::{ContextId, TaskId};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, UserId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

pub type AgentId = String;

/// Hard cap on delegation depth (ADR 0006 §`Consequences/Negative`). Above this we
/// reject the child request with [`OrkError::Workflow`] to bound recursion.
pub const MAX_DELEGATION_DEPTH: u8 = 8;

#[derive(Clone, Debug)]
pub struct CallerIdentity {
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AgentContext {
    pub tenant_id: TenantId,
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub cancel: CancellationToken,
    pub caller: CallerIdentity,
    pub push_notification_url: Option<Url>,
    pub trace_ctx: Option<String>,
    pub context_id: Option<ContextId>,
    /// Workflow run input JSON (passed to integration tools; ADR 0002 LocalAgent parity).
    pub workflow_input: Value,
    /// When executing a `for_each` step: variable name and current element for tool params.
    pub iteration: Option<(String, Value)>,
    /// Number of delegation hops between the originating user request and this context.
    /// `0` for the top-level request; incremented by [`AgentContext::child_for_delegation`].
    pub delegation_depth: u8,
    /// Ordered list of agent ids on the delegation path leading to this context (excluding
    /// the current target). Used for cycle detection in [`AgentContext::child_for_delegation`].
    pub delegation_chain: Vec<AgentId>,
}

impl AgentContext {
    /// Build a child context for an `await:true` delegation to `target`.
    ///
    /// Per ADR 0006:
    /// - the parent's [`CancellationToken`] is cloned so cancelling the parent cancels the child;
    /// - a fresh [`TaskId`] is generated and `parent_task_id` is set to the parent's task;
    /// - `delegation_depth` is incremented and bounded by [`MAX_DELEGATION_DEPTH`];
    /// - delegating to an agent already in the chain (a cycle) is rejected.
    pub fn child_for_delegation(&self, target: &AgentId) -> Result<Self, OrkError> {
        if self.delegation_depth >= MAX_DELEGATION_DEPTH {
            return Err(OrkError::Workflow(format!(
                "max_delegation_depth ({MAX_DELEGATION_DEPTH}) exceeded delegating to {target}"
            )));
        }
        if self.delegation_chain.iter().any(|a| a == target) {
            return Err(OrkError::Workflow(format!(
                "delegation cycle detected: {target} already in chain {:?}",
                self.delegation_chain
            )));
        }

        let mut chain = self.delegation_chain.clone();
        chain.push(target.clone());

        Ok(Self {
            tenant_id: self.tenant_id,
            task_id: TaskId::new(),
            parent_task_id: Some(self.task_id),
            cancel: self.cancel.clone(),
            caller: self.caller.clone(),
            push_notification_url: None,
            trace_ctx: self.trace_ctx.clone(),
            context_id: self.context_id,
            workflow_input: self.workflow_input.clone(),
            iteration: self.iteration.clone(),
            delegation_depth: self.delegation_depth + 1,
            delegation_chain: chain,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::types::TenantId;
    use uuid::Uuid;

    fn root_ctx(tenant: TenantId) -> AgentContext {
        AgentContext {
            tenant_id: tenant,
            task_id: TaskId::new(),
            parent_task_id: None,
            cancel: CancellationToken::new(),
            caller: CallerIdentity {
                tenant_id: tenant,
                user_id: None,
                scopes: vec![],
            },
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: Value::Null,
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
        }
    }

    #[test]
    fn child_increments_depth_and_records_parent_task() {
        let tenant = TenantId(Uuid::nil());
        let parent = root_ctx(tenant);
        let child = parent
            .child_for_delegation(&"researcher".to_string())
            .expect("first hop allowed");
        assert_eq!(child.delegation_depth, 1);
        assert_eq!(child.parent_task_id, Some(parent.task_id));
        assert_ne!(child.task_id, parent.task_id);
        assert_eq!(child.delegation_chain, vec!["researcher".to_string()]);
    }

    #[test]
    fn child_propagates_cancellation_token() {
        let tenant = TenantId(Uuid::nil());
        let parent = root_ctx(tenant);
        let child = parent
            .child_for_delegation(&"writer".to_string())
            .expect("ok");
        parent.cancel.cancel();
        assert!(
            child.cancel.is_cancelled(),
            "cancelling the parent must cancel the child"
        );
    }

    #[test]
    fn rejects_cycle_in_chain() {
        let tenant = TenantId(Uuid::nil());
        let mut ctx = root_ctx(tenant);
        ctx = ctx.child_for_delegation(&"a".to_string()).unwrap();
        ctx = ctx.child_for_delegation(&"b".to_string()).unwrap();
        let err = ctx.child_for_delegation(&"a".to_string()).unwrap_err();
        assert!(matches!(err, OrkError::Workflow(msg) if msg.contains("cycle")));
    }

    #[test]
    fn rejects_above_max_depth() {
        let tenant = TenantId(Uuid::nil());
        let mut ctx = root_ctx(tenant);
        for i in 0..MAX_DELEGATION_DEPTH {
            ctx = ctx
                .child_for_delegation(&format!("agent-{i}"))
                .expect("under cap");
        }
        let err = ctx
            .child_for_delegation(&"one-too-many".to_string())
            .unwrap_err();
        assert!(matches!(err, OrkError::Workflow(msg) if msg.contains("max_delegation_depth")));
    }
}
