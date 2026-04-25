//! ADR 0010 §`Stdio servers`. End-to-end test against the official
//! `@modelcontextprotocol/server-everything` reference server, spawned
//! over stdio via npx.
//!
//! Gated behind the `mcp-stdio-it` feature so the default `cargo test`
//! invocation stays hermetic — `npx`, network access, and Node.js are
//! prerequisites that are only available on the dedicated integration
//! runners (mirrors ADR 0007's `reference-server-it` switch).
//!
//! Run locally with:
//!
//! ```bash
//! cargo test -p ork-mcp --features mcp-stdio-it -- stdio_everything_server
//! ```

#![cfg(feature = "mcp-stdio-it")]

use std::collections::HashMap;
use std::time::Duration;

use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::workflow::engine::ToolExecutor;
use ork_mcp::{McpClient, McpServerConfig, McpTransportConfig};
use serde_json::json;
use tokio_util::sync::CancellationToken;

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
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_everything_server_echo_round_trip() {
    // The reference server is published as
    // `@modelcontextprotocol/server-everything`. `npx -y` ensures it is
    // fetched on first use without an interactive prompt.
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
    let client = McpClient::from_global_servers(
        vec![cfg],
        Duration::from_secs(60),
        Duration::from_secs(60),
        reqwest::Client::new(),
    );
    let tenant = TenantId::new();

    // Warm the descriptor cache. If npx / node / network are missing on
    // the runner this is where the failure surfaces, with a clear
    // Integration error.
    client
        .refresh_for_tenant(tenant)
        .await
        .expect("warm-up refresh against everything-server failed");

    let tools = client.list_tools_for_tenant(tenant);
    assert!(
        tools.iter().any(|d| d.tool_name == "echo"),
        "everything-server must expose an `echo` tool, got: {:?}",
        tools.iter().map(|d| &d.tool_name).collect::<Vec<_>>()
    );

    // Round-trip the echo tool. The reference server returns
    // `{"content": [{"type": "text", "text": "Echo: <message>"}], ...}`.
    let result = client
        .execute(
            &test_ctx(tenant),
            "mcp:everything.echo",
            &json!({"message": "hi"}),
        )
        .await
        .expect("mcp:everything.echo call failed");

    let serialised = serde_json::to_string(&result).unwrap();
    assert!(
        serialised.contains("hi"),
        "echo tool must echo back the input message; serialised result: {serialised}"
    );

    client.shutdown();
}
