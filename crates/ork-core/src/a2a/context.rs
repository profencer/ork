use std::fmt;
use std::sync::Arc;

use ork_a2a::{ContextId, TaskId};
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, UserId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::ports::artifact_store::ArtifactStore;

pub type AgentId = String;

/// Hard cap on delegation depth (ADR 0006 §`Consequences/Negative`). Above this we
/// reject the child request with [`OrkError::Workflow`] to bound recursion.
pub const MAX_DELEGATION_DEPTH: u8 = 8;

#[derive(Clone, Debug, Default)]
pub struct CallerIdentity {
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub scopes: Vec<String>,
    /// ADR-0020 §`Tenant id propagation across delegation`: ordered list of
    /// tenant ids this request has been bound to so far. Canonical default
    /// (set by `auth_middleware` for tokens that omit `tid_chain`) is
    /// `[tenant_id]` — `chain.len() == 1` ⇔ no trust-boundary crossing.
    /// Phase B extends this on outbound `child_for_delegation` when the
    /// target's tenant differs from the source.
    pub tenant_chain: Vec<TenantId>,
    /// ADR-0020 §`Mesh trust`: defaults to [`TrustTier::Internal`].
    pub trust_tier: TrustTier,
    /// ADR-0020 §`Mesh trust`: defaults to [`TrustClass::User`].
    pub trust_class: TrustClass,
    /// ADR-0020: present when `trust_class == Agent` (set by ork during
    /// outbound delegation). The local agent id whose ork minted the token.
    pub agent_id: Option<AgentId>,
}

/// Per-step LLM provider/model overrides carried on [`AgentContext`].
///
/// Populated by [`crate::workflow::engine::WorkflowEngine`] when the active
/// [`crate::models::workflow::WorkflowStep`] declares `provider:` or
/// `model:`. Highest precedence in the ADR 0012 §`Selection` resolution
/// chain (step → agent → tenant default → operator default); a `None`
/// field falls through to the agent config, then the tenant default, then
/// the operator default inside `ork_llm::router::LlmRouter::resolve`.
#[derive(Clone, Debug, Default)]
pub struct StepLlmOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone)]
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
    /// ADR 0012 §`Selection`: per-step provider/model overrides set by the
    /// workflow engine. `None` means "no step-level override active";
    /// downstream resolution falls through to the agent config and then
    /// the tenant/operator catalog defaults.
    ///
    /// Not propagated by [`AgentContext::child_for_delegation`] — a
    /// delegated child task runs its own step (or none, for ad-hoc
    /// peer calls) and re-resolves its overrides from scratch.
    pub step_llm_overrides: Option<StepLlmOverrides>,
    /// ADR-0016: shared blob store for tool spillover and artifact tools.
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    /// Public API origin for proxy URLs when `presign_get` is unavailable (same as
    /// [`crate::embeds::EmbedContext::artifact_public_base`]).
    pub artifact_public_base: Option<String>,
}

impl fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentContext")
            .field("tenant_id", &self.tenant_id)
            .field("task_id", &self.task_id)
            .field("parent_task_id", &self.parent_task_id)
            .field("cancel", &self.cancel)
            .field("caller", &self.caller)
            .field("push_notification_url", &self.push_notification_url)
            .field("trace_ctx", &self.trace_ctx)
            .field("context_id", &self.context_id)
            .field("workflow_input", &self.workflow_input)
            .field("iteration", &self.iteration)
            .field("delegation_depth", &self.delegation_depth)
            .field("delegation_chain", &self.delegation_chain)
            .field("step_llm_overrides", &self.step_llm_overrides)
            .field(
                "artifact_store",
                &self.artifact_store.as_ref().map(|_| "<set>"),
            )
            .field("artifact_public_base", &self.artifact_public_base)
            .finish()
    }
}

impl AgentContext {
    /// Build a child context for an `await:true` delegation to `target` running
    /// under `target_tenant`. For ADR-0006 baseline (same-tenant) callers,
    /// pass `parent_ctx.tenant_id`; ADR-0020 cross-tenant delegations pass the
    /// destination's tenant id (resolved from its [`AgentCard`]).
    ///
    /// Per ADR 0006:
    /// - the parent's [`CancellationToken`] is cloned so cancelling the parent cancels the child;
    /// - a fresh [`TaskId`] is generated and `parent_task_id` is set to the parent's task;
    /// - `delegation_depth` is incremented and bounded by [`MAX_DELEGATION_DEPTH`];
    /// - delegating to an agent already in the chain (a cycle) is rejected.
    ///
    /// Per ADR 0020 §`Tenant id propagation across delegation`:
    /// - When `target_tenant == self.tenant_id` the call is in-tenant and
    ///   `caller.tenant_chain` is left unchanged.
    /// - When `target_tenant != self.tenant_id` the call crosses a trust
    ///   boundary; the new target tenant is appended to `caller.tenant_chain`
    ///   so downstream `tenant:admin` / `agent:<x>:delegate` checks can see
    ///   the full ordered transit. The chain invariant is `chain[-1] ==
    ///   ctx.tenant_id` (Phase A's middleware seeds the canonical default
    ///   `[tenant_id]` on inbound).
    ///
    /// Cycle and depth-cap rejections return [`OrkError::Validation`] (not
    /// `Workflow`) so the LLM tool loop can recover from a mis-routed
    /// `agent_call` per ADR-0010 §`Tool error semantics`. They describe a
    /// bad request from the model, not an engine-internal failure.
    pub fn child_for_delegation(
        &self,
        target: &AgentId,
        target_tenant: TenantId,
    ) -> Result<Self, OrkError> {
        if self.delegation_depth >= MAX_DELEGATION_DEPTH {
            return Err(OrkError::Validation(format!(
                "max_delegation_depth ({MAX_DELEGATION_DEPTH}) exceeded delegating to {target}"
            )));
        }
        if self.delegation_chain.iter().any(|a| a == target) {
            return Err(OrkError::Validation(format!(
                "delegation cycle detected: {target} already in chain {:?}",
                self.delegation_chain
            )));
        }

        let mut chain = self.delegation_chain.clone();
        chain.push(target.clone());

        let mut child_caller = self.caller.clone();
        if target_tenant != self.tenant_id
            && child_caller.tenant_chain.last() != Some(&target_tenant)
        {
            child_caller.tenant_chain.push(target_tenant);
        }

        Ok(Self {
            tenant_id: target_tenant,
            task_id: TaskId::new(),
            parent_task_id: Some(self.task_id),
            cancel: self.cancel.clone(),
            caller: child_caller,
            push_notification_url: None,
            trace_ctx: self.trace_ctx.clone(),
            context_id: self.context_id,
            workflow_input: self.workflow_input.clone(),
            iteration: self.iteration.clone(),
            delegation_depth: self.delegation_depth + 1,
            delegation_chain: chain,
            step_llm_overrides: None,
            artifact_store: self.artifact_store.clone(),
            artifact_public_base: self.artifact_public_base.clone(),
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
                // Match `auth_middleware`'s canonical default per ADR-0020 M1.
                tenant_chain: vec![tenant],
                ..CallerIdentity::default()
            },
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: Value::Null,
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides: None,
            artifact_store: None,
            artifact_public_base: None,
        }
    }

    #[test]
    fn child_increments_depth_and_records_parent_task() {
        let tenant = TenantId(Uuid::nil());
        let parent = root_ctx(tenant);
        let child = parent
            .child_for_delegation(&"researcher".to_string(), tenant)
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
            .child_for_delegation(&"writer".to_string(), tenant)
            .expect("ok");
        parent.cancel.cancel();
        assert!(
            child.cancel.is_cancelled(),
            "cancelling the parent must cancel the child"
        );
    }

    /// ADR-0020 §`Tenant id propagation across delegation`: same-tenant
    /// hops do NOT extend `tenant_chain`. The chain only grows when the
    /// target's tenant differs from the source.
    #[test]
    fn same_tenant_hop_leaves_tenant_chain_unchanged() {
        let tenant = TenantId(Uuid::nil());
        let parent = root_ctx(tenant);
        let child = parent
            .child_for_delegation(&"in-tenant".to_string(), tenant)
            .expect("ok");
        assert_eq!(
            child.caller.tenant_chain,
            vec![tenant],
            "same-tenant delegation must not extend the chain"
        );
        assert_eq!(child.tenant_id, tenant);
    }

    /// Cross-tenant hop appends the target tenant to the caller's chain
    /// and re-binds `ctx.tenant_id` to the target. Two cross-tenant hops
    /// in a row produce a length-3 chain `[origin, mid, leaf]`.
    #[test]
    fn cross_tenant_hop_extends_tenant_chain() {
        let origin = TenantId(Uuid::from_u128(0xaaaa));
        let mid = TenantId(Uuid::from_u128(0xbbbb));
        let leaf = TenantId(Uuid::from_u128(0xcccc));
        let root = root_ctx(origin);
        let hop1 = root
            .child_for_delegation(&"a".to_string(), mid)
            .expect("hop1");
        assert_eq!(hop1.tenant_id, mid);
        assert_eq!(hop1.caller.tenant_chain, vec![origin, mid]);

        let hop2 = hop1
            .child_for_delegation(&"b".to_string(), leaf)
            .expect("hop2");
        assert_eq!(hop2.tenant_id, leaf);
        assert_eq!(hop2.caller.tenant_chain, vec![origin, mid, leaf]);
    }

    /// Idempotency: if the chain already terminates with `target_tenant`
    /// (e.g. caller passed the wrong target tenant explicitly), do not
    /// double-append. Defensive — production paths shouldn't trigger this.
    #[test]
    fn cross_tenant_hop_does_not_double_append() {
        let origin = TenantId(Uuid::from_u128(0xaaaa));
        let mid = TenantId(Uuid::from_u128(0xbbbb));
        let root = root_ctx(origin);
        let hop1 = root
            .child_for_delegation(&"a".to_string(), mid)
            .expect("hop1");
        // Now `hop1.tenant_id == mid` and `chain == [origin, mid]`. A
        // (hypothetical) caller asking to delegate to mid again from
        // hop1's perspective would NOT cross a tenant boundary —
        // same-tenant rule applies.
        let hop2 = hop1
            .child_for_delegation(&"b".to_string(), mid)
            .expect("hop2");
        assert_eq!(hop2.caller.tenant_chain, vec![origin, mid]);
    }

    /// ADR-0010 §`Tool error semantics`: the `agent_call` / `peer_*` tool
    /// loop classifies `OrkError::Workflow` as fatal (it kills the step)
    /// but `OrkError::Validation` as recoverable (the LLM gets the error
    /// back as a `Tool`-role message and can pick a different agent).
    /// Cycle / depth-cap rejections describe a *bad request* (the LLM tried
    /// to delegate somewhere it isn't allowed to), not an engine-internal
    /// failure, so they must be `Validation` — otherwise a single mis-step
    /// in the LLM's tool plan kills the whole workflow step. Regression for
    /// the stage-4 demo failure
    /// `synthesize (failed): workflow error: delegation cycle detected:
    /// synthesizer already in chain ["synthesizer"]`.
    #[test]
    fn rejects_cycle_in_chain_as_validation_error() {
        let tenant = TenantId(Uuid::nil());
        let mut ctx = root_ctx(tenant);
        ctx = ctx.child_for_delegation(&"a".to_string(), tenant).unwrap();
        ctx = ctx.child_for_delegation(&"b".to_string(), tenant).unwrap();
        let err = ctx
            .child_for_delegation(&"a".to_string(), tenant)
            .unwrap_err();
        assert!(
            matches!(err, OrkError::Validation(ref msg) if msg.contains("cycle")),
            "cycle errors must be recoverable Validation errors per ADR-0010, \
             got: {err:?}"
        );
    }

    #[test]
    fn child_does_not_propagate_step_llm_overrides() {
        // Per ADR 0012 §`Selection`: step-level overrides scope to the
        // step that owns them. A delegated child task runs in its own
        // step (or none) and re-resolves from the agent / tenant /
        // operator chain.
        let tenant = TenantId(Uuid::nil());
        let mut parent = root_ctx(tenant);
        parent.step_llm_overrides = Some(StepLlmOverrides {
            provider: Some("step-only".into()),
            model: Some("custom-model".into()),
        });
        let child = parent
            .child_for_delegation(&"writer".to_string(), tenant)
            .expect("ok");
        assert!(child.step_llm_overrides.is_none());
    }

    #[test]
    fn rejects_above_max_depth() {
        let tenant = TenantId(Uuid::nil());
        let mut ctx = root_ctx(tenant);
        for i in 0..MAX_DELEGATION_DEPTH {
            ctx = ctx
                .child_for_delegation(&format!("agent-{i}"), tenant)
                .expect("under cap");
        }
        let err = ctx
            .child_for_delegation(&"one-too-many".to_string(), tenant)
            .unwrap_err();
        assert!(
            matches!(err, OrkError::Validation(ref msg) if msg.contains("max_delegation_depth")),
            "depth-cap errors must be recoverable Validation errors per \
             ADR-0010 (same rationale as the cycle test above), got: {err:?}"
        );
    }
}
