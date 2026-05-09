//! `.gate(..)` omits tools from the catalog when predicate is false (ADR-0051 §`dynamic_tools`).

use std::collections::HashMap;
use std::sync::Arc;

use ork_agents::tool_catalog::ToolCatalogBuilder;
use ork_common::types::TenantId;
use ork_core::a2a::AgentContext;
use ork_core::models::agent::AgentConfig;
use ork_core::ports::tool_def::ToolDef;
use ork_tool::{IntoToolDef, tool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize, JsonSchema)]
struct Empty {}

#[derive(Debug, Serialize, JsonSchema)]
struct Out {
    ok: bool,
}

fn gated_invoice_tool() -> Arc<dyn ToolDef> {
    tool("send_invoice")
        .description("billing tool")
        .input::<Empty>()
        .output::<Out>()
        .gate(|ctx| {
            ctx.agent_context
                .caller
                .scopes
                .contains(&"billing.write".to_string())
        })
        .execute(|_, _| async { Ok(Out { ok: true }) })
        .into_tool_def()
}

fn ctx_with_scopes(scopes: Vec<String>) -> AgentContext {
    let tenant = TenantId::new();
    ork_core::a2a::AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: ork_core::a2a::CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes,
            ..ork_core::a2a::CallerIdentity::default()
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: json!({}),
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

fn agent_config_allow_star() -> AgentConfig {
    AgentConfig {
        id: "planner".into(),
        name: "Planner".into(),
        description: "x".into(),
        system_prompt: "x".into(),
        tools: vec!["*".into()],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 256,
        max_tool_iterations: 8,
        max_parallel_tool_calls: 4,
        max_tool_result_bytes: 16_384,
        expose_reasoning: false,
    }
}

#[tokio::test]
async fn gated_tool_hidden_without_scope() {
    let mut m = HashMap::new();
    m.insert("send_invoice".into(), gated_invoice_tool());
    let catalog = ToolCatalogBuilder::new().with_native_tools(Arc::new(m));
    let config = agent_config_allow_star();

    let ctx_ok = ctx_with_scopes(vec!["billing.write".into()]);
    let tools_ok = catalog
        .for_agent(&ctx_ok, &config)
        .await
        .expect("catalog ok");
    assert!(
        tools_ok.iter().any(|t| t.id() == "send_invoice"),
        "with billing.write the tool must appear"
    );

    let ctx_denied = ctx_with_scopes(vec!["other.read".into()]);
    let tools_denied = catalog
        .for_agent(&ctx_denied, &config)
        .await
        .expect("catalog ok");
    assert!(
        !tools_denied.iter().any(|t| t.id() == "send_invoice"),
        "without billing.write the tool must be omitted"
    );
}
