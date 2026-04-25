//! ADR 0010 §`Streamable-HTTP servers`. Default-on integration test:
//! we boot a tiny rmcp `EchoServer` behind axum + `StreamableHttpService`
//! on a random localhost port and round-trip through the production
//! [`McpClient`] code path.
//!
//! Unlike `stdio_everything_server.rs` this fixture has zero external
//! dependencies (no node, no docker, no network), so it stays in the
//! default `cargo test` matrix and acts as our reference contract test
//! for the `streamable_http` transport branch in
//! [`ork_mcp::transport`](../src/transport.rs).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::workflow::engine::ToolExecutor;
use ork_mcp::{McpClient, McpServerConfig, McpTransportConfig};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
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
        step_llm_overrides: None,
    }
}

/// Argument schema for the `echo` tool. Two fields so the JSON-Schema
/// emitted by `#[tool]` is non-trivially structured (so we exercise the
/// schema path in [`McpClient::refresh_for_tenant`] too).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    /// Free-form payload the server will echo back verbatim.
    msg: String,
}

/// Reference rmcp server used by the in-process integration test. The
/// `#[tool_router]` macro generates the registry; `#[tool_handler]`
/// wires it into [`ServerHandler`] so `tools/list` and `tools/call`
/// just work over StreamableHttp.
#[derive(Debug, Clone)]
struct EchoServer {
    tool_router: ToolRouter<Self>,
}

impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EchoServer {
    /// Echoes the input message back. Used end-to-end by the test below
    /// to assert the round-trip is byte-faithful.
    #[tool(description = "Echo back the input message")]
    async fn echo(&self, Parameters(EchoArgs { msg }): Parameters<EchoArgs>) -> String {
        msg
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("ork-mcp in-process test server".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Spawn the axum server, returning the bound port + a token the caller
/// can cancel to trigger graceful shutdown. We bind on `127.0.0.1:0` so
/// concurrent test runs don't fight over a fixed port.
async fn spawn_echo_server() -> (u16, CancellationToken, tokio::task::JoinHandle<()>) {
    let cancel = CancellationToken::new();
    let service: StreamableHttpService<EchoServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(EchoServer::new()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: None,
                cancellation_token: cancel.child_token(),
                ..Default::default()
            },
        );

    let app = Router::new().nest_service("/mcp", service);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0 for test server");
    let port = listener.local_addr().expect("local_addr").port();

    let cancel_for_serve = cancel.clone();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel_for_serve.cancelled_owned().await })
            .await;
    });

    (port, cancel, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamable_http_in_process_round_trip() {
    let (port, server_cancel, server_handle) = spawn_echo_server().await;

    // Build the client against the just-bound port. The server id
    // (`echo_server`) is what shows up in the `mcp:<server>.<tool>`
    // namespace; we'll use it below to call `mcp:echo_server.echo`.
    let cfg = McpServerConfig {
        id: "echo_server".into(),
        transport: McpTransportConfig::StreamableHttp {
            url: url::Url::parse(&format!("http://127.0.0.1:{port}/mcp"))
                .expect("valid loopback URL"),
            auth: ork_mcp::McpAuthConfig::None,
        },
    };
    let client = McpClient::from_global_servers(
        vec![cfg],
        Duration::from_secs(60),
        Duration::from_secs(60),
        reqwest::Client::new(),
    );

    let tenant = TenantId::new();

    // Warm the descriptor cache. Exercises `tools/list`, the rmcp
    // `initialize` handshake, and our session-pool single-flight path
    // all in one shot.
    client
        .refresh_for_tenant(tenant)
        .await
        .expect("refresh_for_tenant against in-process echo_server failed");

    let tools = client.list_tools_for_tenant(tenant);
    assert!(
        tools.iter().any(|d| d.tool_name == "echo"),
        "in-process echo_server must expose an `echo` tool, got: {:?}",
        tools.iter().map(|d| &d.tool_name).collect::<Vec<_>>()
    );

    // Round-trip the echo tool. The rmcp default content wrapper
    // serialises the returned `String` as a `text` content block, so
    // the literal payload (`hello`) appears verbatim in the JSON.
    let result = client
        .execute(
            &test_ctx(tenant),
            "mcp:echo_server.echo",
            &json!({"msg": "hello"}),
        )
        .await
        .expect("mcp:echo_server.echo call failed");

    let serialised = serde_json::to_string(&result).expect("serialise CallToolResult");
    assert!(
        serialised.contains("hello"),
        "echo tool must echo `hello` verbatim; serialised result: {serialised}"
    );

    client.shutdown();
    server_cancel.cancel();
    let _ = server_handle.await;
}
