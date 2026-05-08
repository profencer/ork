use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use ork_common::error::OrkError;
use tracing::debug;

use ork_core::a2a::AgentContext;
use ork_core::ports::integration::{RepoQuery, SourceControlAdapter};
use ork_core::workflow::engine::ToolExecutor;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_plane::ToolPlaneExecutor;
    use ork_common::types::TenantId;
    use ork_core::a2a::CallerIdentity;
    use std::collections::HashMap;
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

    fn empty_plane() -> ToolPlaneExecutor {
        ToolPlaneExecutor::new(Arc::new(HashMap::new()), None, None)
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
                // ADR-0021 §`Defaults` End-user equivalent: wildcards
                // for tool / MCP so the existing routing tests focus on
                // the `mcp:` / `peer_*` arms, not the scope gate. A
                // dedicated deny test below pins the gate behaviour.
                scopes: vec!["tool:*:invoke".into(), "tool:mcp:*:invoke".into()],
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
        }
    }

    #[tokio::test]
    async fn composite_routes_mcp_prefix_to_mcp_when_set() {
        let stub = StubExecutor::new();
        let exec: Arc<dyn ToolExecutor> = stub.clone();
        let composite = ToolPlaneExecutor::new(Arc::new(HashMap::new()), None, Some(exec));
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
        let composite = empty_plane();
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
        let composite = ToolPlaneExecutor::new(Arc::new(HashMap::new()), None, Some(exec));
        let tenant = TenantId::new();

        let res = composite
            .execute(&test_ctx(tenant), "mcp:x.y", &serde_json::json!({}))
            .await
            .expect("mcp arm must run before integration fallthrough");
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        assert_eq!(res["stub"], "mcp:x.y");
    }
}
