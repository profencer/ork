//! ADR 0047 — optional MCP stdio round-trip through Rig + `LocalAgent`.
//!
//! Run with:
//! `cargo test -p ork-agents --features mcp-stdio-it -- rig_engine_mcp_smoke`

#![cfg(feature = "mcp-stdio-it")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent};
use ork_agents::local::LocalAgent;
use ork_agents::tool_catalog::ToolCatalogBuilder;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    TokenUsage, ToolCall,
};
use ork_core::workflow::engine::ToolExecutor;
use ork_mcp::{McpClient, McpServerConfig, McpTransportConfig};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct ScriptedLlm {
    streams: Mutex<Vec<Vec<ChatStreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedLlm {
    fn new(streams: Vec<Vec<ChatStreamEvent>>) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().rev().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        self.requests.lock().await.push(request);
        let events = self
            .streams
            .lock()
            .await
            .pop()
            .unwrap_or_else(|| vec![done(FinishReason::Stop, "stub")]);
        Ok(Box::pin(async_stream::stream! {
            for ev in events {
                yield Ok(ev);
            }
        }))
    }

    fn provider_name(&self) -> &str {
        "mcp-rig-smoke"
    }
}

fn done(reason: FinishReason, model: &str) -> ChatStreamEvent {
    ChatStreamEvent::Done {
        usage: TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        model: model.into(),
        finish_reason: reason,
    }
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        id: "mcp-rig-smoke".into(),
        name: "McpRigSmoke".into(),
        description: "smoke".into(),
        system_prompt: "test".into(),
        tools: vec!["mcp:everything.echo".into()],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 256,
        max_tool_iterations: 8,
        max_parallel_tool_calls: 2,
        max_tool_result_bytes: 65_536,
        expose_reasoning: false,
    }
}

fn test_ctx(tenant: TenantId) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rig_engine_mcp_echo_round_trip() {
    let cfg = McpServerConfig {
        id: "everything".into(),
        transport: McpTransportConfig::Stdio {
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
            env: HashMap::new(),
        },
    };
    let client = Arc::new(McpClient::from_global_servers(
        vec![cfg],
        Duration::from_secs(60),
        Duration::from_secs(60),
        reqwest::Client::new(),
    ));
    let tenant = TenantId::new();
    client
        .refresh_for_tenant(tenant)
        .await
        .expect("MCP warm-up");

    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "mcp_echo".into(),
                name: "mcp:everything.echo".into(),
                arguments: json!({"message": "rig-mcp"}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("done".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));

    let ctx = test_ctx(tenant);
    let cat: Arc<dyn ork_agents::tool_catalog::McpToolCatalog> = client.clone();
    let exec: Arc<dyn ToolExecutor> = client.clone();
    let agent = LocalAgent::new(agent_config(), &CardEnrichmentContext::minimal(), llm)
        .with_tool_catalog(ToolCatalogBuilder::new().with_mcp_plane(cat, exec));

    let msg = AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: "call echo".into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(ctx.task_id),
        context_id: None,
        metadata: None,
    };

    let mut stream = agent.send_stream(ctx, msg).await.expect("stream");
    let mut final_text = String::new();
    while let Some(ev) = stream.next().await {
        if let Ok(AgentEvent::Message(m)) = ev {
            for p in m.parts {
                if let Part::Text { text, .. } = p {
                    final_text.push_str(&text);
                }
            }
        }
    }

    assert!(
        final_text.contains("rig-mcp") || final_text.to_lowercase().contains("echo"),
        "expected echo content in final agent message, got: {final_text:?}"
    );

    client.shutdown();
}
