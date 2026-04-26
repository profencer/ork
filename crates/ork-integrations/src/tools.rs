use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use ork_common::error::OrkError;
use tracing::debug;

use ork_core::a2a::AgentContext;
use ork_core::ports::integration::{RepoQuery, SourceControlAdapter};
use ork_core::ports::llm::ToolDescriptor;
use ork_core::workflow::engine::ToolExecutor;

use crate::agent_call::AgentCallToolExecutor;
use crate::artifact_tools::{self, ArtifactToolExecutor};
use crate::code_tools::CodeToolExecutor;

/// Registry of tools available to agents, backed by integration adapters.
pub struct IntegrationToolExecutor {
    adapters: HashMap<String, Arc<dyn SourceControlAdapter>>,
}

impl IntegrationToolExecutor {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    pub fn register_adapter(&mut self, name: &str, adapter: Arc<dyn SourceControlAdapter>) {
        self.adapters.insert(name.to_string(), adapter);
    }

    #[must_use]
    pub fn descriptor(name: &str) -> Option<ToolDescriptor> {
        let repo_window = serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"}
            }
        });
        let tag_window = serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {"type": "string"},
                "repo": {"type": "string"},
                "from_tag": {"type": "string"},
                "to_tag": {"type": "string"}
            },
            "required": ["from_tag", "to_tag"]
        });
        match name {
            "github_recent_activity" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch recent GitHub commits, pull requests, and issues.".into(),
                parameters: repo_window,
            }),
            "gitlab_recent_activity" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch recent GitLab commits, merge requests, and issues.".into(),
                parameters: repo_window,
            }),
            "github_merged_prs" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch GitHub pull requests merged between two tags.".into(),
                parameters: tag_window,
            }),
            "gitlab_merged_prs" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch GitLab merge requests merged between two tags.".into(),
                parameters: tag_window,
            }),
            "github_pipelines" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch GitHub pipeline/check information for a repository.".into(),
                parameters: repo_window,
            }),
            "gitlab_pipelines" => Some(ToolDescriptor {
                name: name.into(),
                description: "Fetch GitLab pipeline information for a repository.".into(),
                parameters: repo_window,
            }),
            _ => artifact_tools::integration_descriptor(name),
        }
    }
}

impl Default for IntegrationToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for IntegrationToolExecutor {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        debug!(tool = tool_name, "executing integration tool");

        match tool_name {
            "github_recent_activity" => {
                let adapter = self
                    .adapters
                    .get("github")
                    .ok_or_else(|| OrkError::Integration("GitHub adapter not configured".into()))?;
                self.fetch_recent_activity(adapter.as_ref(), input).await
            }
            "gitlab_recent_activity" => {
                let adapter = self
                    .adapters
                    .get("gitlab")
                    .ok_or_else(|| OrkError::Integration("GitLab adapter not configured".into()))?;
                self.fetch_recent_activity(adapter.as_ref(), input).await
            }
            "github_merged_prs" | "gitlab_merged_prs" => {
                let provider = if tool_name.starts_with("github") {
                    "github"
                } else {
                    "gitlab"
                };
                let adapter = self.adapters.get(provider).ok_or_else(|| {
                    OrkError::Integration(format!("{provider} adapter not configured"))
                })?;
                self.fetch_merged_prs(adapter.as_ref(), input).await
            }
            "github_pipelines" | "gitlab_pipelines" => {
                let provider = if tool_name.starts_with("github") {
                    "github"
                } else {
                    "gitlab"
                };
                let adapter = self.adapters.get(provider).ok_or_else(|| {
                    OrkError::Integration(format!("{provider} adapter not configured"))
                })?;
                self.fetch_pipelines(adapter.as_ref(), input).await
            }
            _ => Err(OrkError::Integration(format!("unknown tool: {tool_name}"))),
        }
    }
}

impl IntegrationToolExecutor {
    async fn fetch_recent_activity(
        &self,
        adapter: &dyn SourceControlAdapter,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let owner = input["owner"].as_str().unwrap_or("default").to_string();
        let repo = input["repo"].as_str().unwrap_or("default").to_string();

        let since = Utc::now() - Duration::hours(24);
        let query = RepoQuery {
            owner,
            repo,
            since: Some(since),
            until: None,
            branch: None,
        };

        let commits = adapter
            .list_recent_commits(&query)
            .await
            .unwrap_or_default();
        let prs = adapter
            .list_pull_requests(&query, None)
            .await
            .unwrap_or_default();
        let issues = adapter.list_issues(&query, None).await.unwrap_or_default();

        Ok(serde_json::json!({
            "provider": adapter.provider_name(),
            "commits": commits,
            "pull_requests": prs,
            "issues": issues,
        }))
    }

    async fn fetch_merged_prs(
        &self,
        adapter: &dyn SourceControlAdapter,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let owner = input["owner"].as_str().unwrap_or("default");
        let repo = input["repo"].as_str().unwrap_or("default");
        let from_tag = input["from_tag"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("from_tag is required".into()))?;
        let to_tag = input["to_tag"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("to_tag is required".into()))?;

        let prs = adapter
            .get_merged_prs_between_tags(owner, repo, from_tag, to_tag)
            .await?;

        Ok(serde_json::json!({
            "provider": adapter.provider_name(),
            "merged_pull_requests": prs,
        }))
    }

    async fn fetch_pipelines(
        &self,
        adapter: &dyn SourceControlAdapter,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let owner = input["owner"].as_str().unwrap_or("default").to_string();
        let repo = input["repo"].as_str().unwrap_or("default").to_string();

        let query = RepoQuery {
            owner,
            repo,
            since: None,
            until: None,
            branch: None,
        };

        let pipelines = adapter.list_pipelines(&query).await?;

        Ok(serde_json::json!({
            "provider": adapter.provider_name(),
            "pipelines": pipelines,
        }))
    }
}

/// Routes GitHub/GitLab integration tools, local workspace code tools,
/// the `agent_call` peer-delegation tool from ADR 0006, and (when set)
/// any `mcp:<server>.<tool>` invocation from ADR 0010.
pub struct CompositeToolExecutor {
    integration: Arc<IntegrationToolExecutor>,
    code: Option<Arc<CodeToolExecutor>>,
    agent_call: Option<Arc<AgentCallToolExecutor>>,
    /// ADR-0010 §`Composite routing`. A trait-object so `ork-integrations`
    /// stays unaware of the concrete `ork_mcp::McpClient` (and therefore
    /// of the rmcp dep). Tests can substitute any `ToolExecutor` impl.
    mcp: Option<Arc<dyn ToolExecutor>>,
    /// ADR-0016: optional [`ArtifactToolExecutor`]; if unset, artifact tools error at runtime.
    artifacts: Option<Arc<ArtifactToolExecutor>>,
}

impl CompositeToolExecutor {
    pub fn new(integration: IntegrationToolExecutor, code: Option<Arc<CodeToolExecutor>>) -> Self {
        Self {
            integration: Arc::new(integration),
            code,
            agent_call: None,
            mcp: None,
            artifacts: None,
        }
    }

    /// Attach the `agent_call` tool executor (ADR 0006). When set, calls to
    /// `tool_name == "agent_call"` are dispatched through it; otherwise such calls
    /// fail with [`OrkError::Integration`].
    #[must_use]
    pub fn with_agent_call(mut self, exec: Arc<AgentCallToolExecutor>) -> Self {
        self.agent_call = Some(exec);
        self
    }

    /// Borrow the attached `agent_call` executor; `LocalAgent` uses this to set the
    /// per-call caller context before dispatching a tool batch.
    #[must_use]
    pub fn agent_call(&self) -> Option<&Arc<AgentCallToolExecutor>> {
        self.agent_call.as_ref()
    }

    /// Attach the MCP tool executor (ADR 0010). When set, every tool name
    /// that starts with `mcp:` is routed through it. Without this
    /// builder call such names produce a clear `Integration` error
    /// instead of being silently misrouted to the GitHub/GitLab arm.
    #[must_use]
    pub fn with_mcp(mut self, exec: Arc<dyn ToolExecutor>) -> Self {
        self.mcp = Some(exec);
        self
    }

    /// ADR-0016: attach the artifact tool executor; when `None`, artifact tools
    /// return a clear `Integration` error.
    #[must_use]
    pub fn with_artifacts(mut self, exec: Option<Arc<ArtifactToolExecutor>>) -> Self {
        self.artifacts = exec;
        self
    }
}

#[async_trait]
impl ToolExecutor for CompositeToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        // ADR-0010 §`Composite routing`. The `mcp:` arm runs FIRST so a
        // future bug that registers an `mcp:*` literal in the
        // integration map can't shadow a real MCP tool. Tested by
        // `composite_mcp_takes_priority_over_integration_match` below.
        if tool_name.starts_with("mcp:") {
            let mcp = self.mcp.as_ref().ok_or_else(|| {
                OrkError::Integration(format!(
                    "tool `{tool_name}` is mcp-prefixed but the MCP tool plane is not configured (ADR-0010: set [mcp] in config or per-tenant settings)"
                ))
            })?;
            return mcp.execute(ctx, tool_name, input).await;
        }

        if tool_name == "agent_call" {
            let agent_call = self.agent_call.as_ref().ok_or_else(|| {
                OrkError::Integration(
                    "agent_call tool not configured (ADR-0006 not wired in this build)".into(),
                )
            })?;
            return agent_call.execute(ctx, tool_name, input).await;
        }

        if artifact_tools::is_artifact_tool(tool_name) {
            let exec = self.artifacts.as_ref().ok_or_else(|| {
                OrkError::Integration(
                    "artifact tools not configured (ADR-0016: provision ArtifactStore in process)"
                        .into(),
                )
            })?;
            return exec.execute(ctx, tool_name, input).await;
        }

        // ADR-0006 §`LLM tool surface`. The catalog advertises one descriptor
        // per peer skill named `peer_<agent_id>_<skill_id>`; this arm desugars
        // those into the same one-shot delegation as `agent_call` so the LLM
        // can pick a peer by capability without us having to register every
        // possible target as its own arm. Without this, the integration arm
        // below catches `peer_*` and returns `unknown tool: …`.
        if tool_name.starts_with("peer_") {
            let agent_call = self.agent_call.as_ref().ok_or_else(|| {
                OrkError::Integration(format!(
                    "peer tool `{tool_name}` was advertised by the catalog but agent_call is not configured (ADR-0006 not wired in this build)"
                ))
            })?;
            return agent_call.dispatch_peer_tool(ctx, tool_name, input).await;
        }

        if CodeToolExecutor::is_code_tool(tool_name) {
            if let Some(c) = &self.code {
                return c.execute(ctx, tool_name, input).await;
            }
            return Err(OrkError::Integration(
                "code/workspace tools not configured (add repositories in config)".into(),
            ));
        }

        self.integration.execute(ctx, tool_name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::types::TenantId;
    use ork_core::a2a::CallerIdentity;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    /// Captures every `execute` call so assertions can verify which arm
    /// of the composite router fired. Stand-in for `McpClient` so this
    /// test file doesn't need to depend on `ork-mcp` (which would close
    /// the cycle described in ADR-0010 §`Composite routing`).
    struct StubExecutor {
        calls: Arc<AtomicUsize>,
        last_tool: Arc<std::sync::Mutex<Option<String>>>,
    }

    impl StubExecutor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Arc::new(AtomicUsize::new(0)),
                last_tool: Arc::new(std::sync::Mutex::new(None)),
            })
        }
    }

    #[async_trait]
    impl ToolExecutor for StubExecutor {
        async fn execute(
            &self,
            _ctx: &AgentContext,
            tool_name: &str,
            _input: &serde_json::Value,
        ) -> Result<serde_json::Value, OrkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_tool.lock().unwrap() = Some(tool_name.to_string());
            Ok(serde_json::json!({"stub": tool_name}))
        }
    }

    fn empty_composite() -> CompositeToolExecutor {
        CompositeToolExecutor::new(IntegrationToolExecutor::new(), None)
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
        }
    }

    #[tokio::test]
    async fn composite_routes_mcp_prefix_to_mcp_when_set() {
        let stub = StubExecutor::new();
        let exec: Arc<dyn ToolExecutor> = stub.clone();
        let composite = empty_composite().with_mcp(exec);
        let tenant = TenantId::new();

        let result = composite
            .execute(
                &test_ctx(tenant),
                "mcp:atlassian.search_jira",
                &serde_json::json!({}),
            )
            .await
            .expect("mcp call must succeed via stub");

        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stub.last_tool.lock().unwrap().as_deref(),
            Some("mcp:atlassian.search_jira"),
            "MCP arm must receive the *qualified* tool name unchanged"
        );
        assert_eq!(result["stub"], "mcp:atlassian.search_jira");
    }

    #[tokio::test]
    async fn composite_returns_clear_error_for_mcp_prefix_when_unset() {
        let composite = empty_composite();
        let tenant = TenantId::new();
        let err = composite
            .execute(&test_ctx(tenant), "mcp:foo.bar", &serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            OrkError::Integration(msg) => {
                assert!(msg.contains("mcp"));
                assert!(
                    msg.contains("not configured"),
                    "error must explain the MCP plane is missing, got `{msg}`"
                );
                assert!(
                    msg.contains("ADR-0010"),
                    "error must point operators at ADR-0010, got `{msg}`"
                );
            }
            other => panic!("expected Integration error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn composite_mcp_takes_priority_over_integration_match() {
        let stub = StubExecutor::new();
        let exec: Arc<dyn ToolExecutor> = stub.clone();
        let composite = empty_composite().with_mcp(exec);
        let tenant = TenantId::new();

        let res = composite
            .execute(&test_ctx(tenant), "mcp:x.y", &serde_json::json!({}))
            .await
            .expect("mcp arm must run before integration fallthrough");
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        assert_eq!(res["stub"], "mcp:x.y");
    }
}
