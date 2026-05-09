//! ADR-0020 §`Tenant id propagation across delegation`: unit tests for
//! [`enforce_delegation_policy`].
//!
//! The gate has two layers.
//!
//! **Always:** caller carries `agent:<target>:delegate` (exact, or via
//! a `*` wildcard, or via `tenant:admin`). Without it, every
//! delegation is rejected.
//!
//! **Cross-tenant only:** in addition, caller has `tenant:admin` OR
//! the destination card declares
//! `mesh-trust.params.accepts_external_tenants = true`.
//!
//! All rejections are `OrkError::Validation` (recoverable per ADR-0010
//! §`Tool error semantics`) so the LLM tool loop can pick a different
//! target rather than killing the workflow step.

use ork_a2a::extensions::{EXT_MESH_TRUST, PARAM_ACCEPTS_EXTERNAL_TENANTS};
use ork_a2a::{AgentCapabilities, AgentCard, AgentExtension, AgentSkill};
use ork_common::auth::{TENANT_ADMIN_SCOPE, agent_delegate_scope};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::workflow::delegation::enforce_delegation_policy;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn ctx_with_scopes(tenant: TenantId, scopes: &[&str]) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            tenant_chain: vec![tenant],
            ..CallerIdentity::default()
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

fn card(name: &str, accepts_external: Option<bool>) -> AgentCard {
    let extensions = accepts_external.map(|b| {
        vec![AgentExtension {
            uri: EXT_MESH_TRUST.into(),
            description: None,
            params: Some(
                json!({ PARAM_ACCEPTS_EXTERNAL_TENANTS: b })
                    .as_object()
                    .cloned()
                    .expect("object"),
            ),
        }]
    });
    AgentCard {
        name: name.into(),
        description: "x".into(),
        version: "0.1.0".into(),
        url: None,
        provider: None,
        capabilities: AgentCapabilities {
            streaming: false,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "default".into(),
            name: name.into(),
            description: "x".into(),
            tags: vec![],
            examples: vec![],
            input_modes: None,
            output_modes: None,
        }],
        security_schemes: None,
        security: None,
        extensions,
    }
}

/// Caller has the exact `agent:<target>:delegate` scope and the target
/// runs in the parent's tenant — same-tenant happy path.
#[test]
fn same_tenant_with_exact_scope_is_allowed() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(tenant, &[&agent_delegate_scope(&target)]);
    enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect("same-tenant + exact scope must pass");
}

/// Wildcard agent scope (`agent:*:delegate`) covers any concrete target
/// in the same tenant.
#[test]
fn same_tenant_with_wildcard_scope_is_allowed() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(tenant, &["agent:*:delegate"]);
    enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect("wildcard delegate must pass for any agent");
}

/// `tenant:admin` is a super-scope; it implies any `agent:*:delegate`.
#[test]
fn same_tenant_with_admin_scope_is_allowed() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(tenant, &[TENANT_ADMIN_SCOPE]);
    enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect("tenant:admin must pass");
}

/// No matching scope → recoverable `Validation` rejection. The LLM tool
/// loop relies on this being recoverable (ADR-0010 §`Tool error
/// semantics`) so it can pick a different agent.
#[test]
fn missing_delegate_scope_is_validation_error() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(tenant, &["agent:other:invoke", "tool:agent_call:invoke"]);
    let err = enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect_err("missing scope must reject");
    match err {
        OrkError::Validation(msg) => {
            assert!(
                msg.contains("agent:reviewer:delegate"),
                "rejection must name the missing scope; got: {msg}"
            );
        }
        other => panic!("expected Validation, got: {other:?}"),
    }
}

/// Cross-tenant call without `tenant:admin` and without
/// `accepts_external_tenants` is rejected. The exact `agent:<target>:delegate`
/// scope is necessary but not sufficient on its own for cross-tenant.
#[test]
fn cross_tenant_without_admin_or_card_consent_is_rejected() {
    let parent_tenant = TenantId(Uuid::from_u128(0xaaaa));
    let target_tenant = TenantId(Uuid::from_u128(0xbbbb));
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(parent_tenant, &[&agent_delegate_scope(&target)]);
    let err = enforce_delegation_policy(&ctx, &target, target_tenant, &card(&target, Some(false)))
        .expect_err("cross-tenant without admin must reject");
    match err {
        OrkError::Validation(msg) => {
            assert!(
                msg.contains("cross-tenant") && msg.contains("tenant:admin"),
                "rejection must explain cross-tenant + admin path; got: {msg}"
            );
        }
        other => panic!("expected Validation, got: {other:?}"),
    }
}

/// Cross-tenant + `tenant:admin` is allowed.
#[test]
fn cross_tenant_with_admin_scope_is_allowed() {
    let parent_tenant = TenantId(Uuid::from_u128(0xaaaa));
    let target_tenant = TenantId(Uuid::from_u128(0xbbbb));
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(parent_tenant, &[TENANT_ADMIN_SCOPE]);
    enforce_delegation_policy(&ctx, &target, target_tenant, &card(&target, None))
        .expect("admin must traverse tenant boundary");
}

/// Cross-tenant + caller has `agent:<target>:delegate` AND the destination
/// card opts in via `accepts_external_tenants: true` — allowed without
/// admin.
#[test]
fn cross_tenant_with_card_consent_is_allowed() {
    let parent_tenant = TenantId(Uuid::from_u128(0xaaaa));
    let target_tenant = TenantId(Uuid::from_u128(0xbbbb));
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(parent_tenant, &[&agent_delegate_scope(&target)]);
    enforce_delegation_policy(&ctx, &target, target_tenant, &card(&target, Some(true)))
        .expect("card opt-in lets a non-admin cross tenants");
}

/// Wildcard `*` (the catch-all "I trust this caller for anything") also
/// satisfies the always-required scope check.
#[test]
fn star_wildcard_scope_satisfies_delegate_check() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    let ctx = ctx_with_scopes(tenant, &["*"]);
    enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect("`*` must pass the always-required check");
}

/// Sanity: a `tool:*:delegate` shape does NOT count as agent-delegate.
/// The wildcard match is conservative (segment-aware).
#[test]
fn tool_wildcard_does_not_grant_agent_delegation() {
    let tenant = TenantId(Uuid::now_v7());
    let target = "reviewer".to_string();
    // `tool:*:delegate` is not a real scope, but make sure if anyone ever
    // grants it they don't accidentally cross the agent-delegation gate.
    let ctx = ctx_with_scopes(tenant, &["tool:*:delegate"]);
    enforce_delegation_policy(&ctx, &target, tenant, &card(&target, None))
        .expect_err("tool:*:delegate must NOT satisfy agent delegation");
}
