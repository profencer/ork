use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::llm::ToolDescriptor;
use ork_core::ports::workspace::RepoWorkspace;
use ork_core::workflow::engine::ToolExecutor;
use serde_json::{Value, json};
use tracing::info;

use crate::workspace::GitRepoWorkspace;

/// Tools: `list_repos`, `code_search`, `read_file`, `list_tree`.
pub struct CodeToolExecutor {
    workspace: Arc<GitRepoWorkspace>,
}

impl CodeToolExecutor {
    pub fn new(workspace: Arc<GitRepoWorkspace>) -> Self {
        Self { workspace }
    }

    pub fn is_code_tool(name: &str) -> bool {
        matches!(
            name,
            "list_repos" | "code_search" | "read_file" | "list_tree"
        )
    }

    #[must_use]
    pub fn descriptors() -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor {
                name: "list_repos".into(),
                description: "List configured source repositories available to this tenant.".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
            ToolDescriptor {
                name: "code_search".into(),
                description: "Search repository contents with ripgrep-style text search.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "query": {"type": "string"},
                        "top_k": {"type": "integer", "minimum": 1, "maximum": 100}
                    },
                    "required": ["repo", "query"]
                }),
            },
            ToolDescriptor {
                name: "read_file".into(),
                description: "Read a file from a configured repository clone.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "path": {"type": "string"},
                        "max_bytes": {"type": "integer", "minimum": 1}
                    },
                    "required": ["repo", "path"]
                }),
            },
            ToolDescriptor {
                name: "list_tree".into(),
                description: "List paths under a repository prefix.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "prefix": {"type": "string"},
                        "max_entries": {"type": "integer", "minimum": 1}
                    },
                    "required": ["repo"]
                }),
            },
        ]
    }
}

#[async_trait]
impl ToolExecutor for CodeToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &Value,
    ) -> Result<Value, OrkError> {
        let tenant_id = ctx.tenant_id;
        match tool_name {
            "list_repos" => {
                info!("tool list_repos: listing configured repositories for the planner");
                let specs = self.workspace.list_specs();
                Ok(json!({ "repositories": specs }))
            }
            "code_search" => {
                let name = repo_name(input).ok_or_else(|| {
                    OrkError::Validation(
                        "code_search requires repo name (input.repo or input.repo.name)".into(),
                    )
                })?;
                let query = search_query(input).ok_or_else(|| {
                    OrkError::Validation(
                        "code_search requires input.query or a quoted query in input.prompt".into(),
                    )
                })?;
                let top_k = input.get("top_k").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
                info!(
                    repository = %name,
                    query = %query,
                    top_k,
                    "tool code_search: updating clone if needed, then ripgrep"
                );
                let hits = self
                    .workspace
                    .code_search(tenant_id, &name, &query, top_k)
                    .await?;
                Ok(json!({ "repository": name, "query": query, "hits": hits }))
            }
            "read_file" => {
                let name = repo_name(input).ok_or_else(|| {
                    OrkError::Validation("read_file requires input.repo or input.repo.name".into())
                })?;
                let path = input
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(String::from)
                    .or_else(|| first_hit_path_from_previous(input))
                    .ok_or_else(|| {
                        OrkError::Validation(
                            "read_file requires input.path or a prior code_search hit".into(),
                        )
                    })?;
                let max_bytes = input
                    .get("max_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(65_536) as usize;
                info!(
                    repository = %name,
                    path = %path,
                    "tool read_file: reading from clone"
                );
                let content = self
                    .workspace
                    .read_file(tenant_id, &name, &path, max_bytes)
                    .await?;
                Ok(json!({
                    "repository": name,
                    "path": path,
                    "content": content,
                }))
            }
            "list_tree" => {
                let name = repo_name(input).ok_or_else(|| {
                    OrkError::Validation("list_tree requires input.repo or input.repo.name".into())
                })?;
                let prefix = input.get("prefix").and_then(|p| p.as_str()).unwrap_or("");
                let max_entries = input
                    .get("max_entries")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500) as usize;
                info!(
                    repository = %name,
                    prefix = %prefix,
                    "tool list_tree: walking files under clone"
                );
                let paths = self
                    .workspace
                    .list_tree(tenant_id, &name, prefix, max_entries)
                    .await?;
                Ok(json!({
                    "repository": name,
                    "prefix": prefix,
                    "paths": paths,
                }))
            }
            _ => Err(OrkError::Integration(format!(
                "unknown code tool: {tool_name}"
            ))),
        }
    }
}

fn repo_name(input: &Value) -> Option<String> {
    if let Some(s) = input.get("repo").and_then(|r| r.as_str()) {
        return Some(s.to_string());
    }
    input
        .get("repo")
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from)
}

fn search_query(input: &Value) -> Option<String> {
    if let Some(q) = input.get("query").and_then(|q| q.as_str())
        && !q.is_empty()
    {
        return Some(q.to_string());
    }
    let prompt = input.get("prompt").and_then(|p| p.as_str())?;
    extract_query_from_prompt(prompt)
}

fn extract_query_from_prompt(prompt: &str) -> Option<String> {
    let lower = prompt.to_ascii_lowercase();
    for needle in ["query \"", "query: \"", "query=\""] {
        if let Some(i) = lower.find(needle) {
            let rel = i + needle.len();
            let rest = &prompt[rel..];
            if let Some(end) = rest.find('"') {
                let q = rest[..end].trim();
                if !q.is_empty() {
                    return Some(q.to_string());
                }
            }
        }
    }
    None
}

fn first_hit_path_from_previous(input: &Value) -> Option<String> {
    let prev = input.get("previous_tool_output")?;
    let hits = prev.get("hits")?.as_array()?;
    hits.first()
        .and_then(|h| h.get("path"))
        .and_then(|p| p.as_str())
        .map(String::from)
}
