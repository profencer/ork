//! Build [`ToolDef`] values for all native (non-MCP, non-peer) tools (ADR-0051).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::tool_def::ToolDef;
use ork_core::workflow::engine::ToolExecutor;
use serde_json::{Value, json};

use ork_tool::DynToolInvoke;

use crate::agent_call::AgentCallToolExecutor;
use crate::artifact_tools::ArtifactToolExecutor;
use crate::code_tools::CodeToolExecutor;
use crate::tools::IntegrationToolExecutor;

fn empty_object_schema() -> Value {
    json!({"type": "object"})
}

/// Wire `dyn ToolExecutor` as [`ToolDef::invoke`] for a fixed tool name.
fn def_from_executor<E: ToolExecutor + ?Sized + 'static>(
    name: impl Into<String>,
    description: impl Into<String>,
    parameters: Value,
    exec: Arc<E>,
) -> Arc<dyn ToolDef> {
    let name = name.into();
    let description = description.into();
    let name_for_closure = name.clone();
    Arc::new(DynToolInvoke::new(
        name,
        description,
        parameters,
        empty_object_schema(),
        Arc::new(move |ctx, input| {
            let exec = exec.clone();
            let n = name_for_closure.clone();
            Box::pin(async move { exec.execute(&ctx, &n, &input).await })
        }),
    ))
}

/// Populate `out` with every native tool backed by the given executors.
#[allow(clippy::too_many_lines)]
pub fn extend_native_tool_map(
    out: &mut HashMap<String, Arc<dyn ToolDef>>,
    integration: Arc<IntegrationToolExecutor>,
    code: Option<Arc<CodeToolExecutor>>,
    artifacts: Option<Arc<ArtifactToolExecutor>>,
    agent_call: Option<Arc<AgentCallToolExecutor>>,
) {
    // --- agent_call (ADR-0006) ---
    if let Some(agent_call) = agent_call {
        let agent_params = json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string"},
                "prompt": {"type": "string"},
                "data": {"type": "object"},
                "await": {"type": "boolean"},
                "stream": {"type": "boolean"}
            },
            "required": ["agent", "prompt"]
        });
        out.insert(
            "agent_call".into(),
            def_from_executor(
                "agent_call",
                "Delegate work to another agent. Pass `agent` and `prompt`; set `await` false for fire-and-forget.",
                agent_params,
                agent_call,
            ),
        );
    }

    // --- GitHub / GitLab integration ---
    let repo_window = json!({
        "type": "object",
        "properties": {
            "owner": {"type": "string"},
            "repo": {"type": "string"}
        }
    });
    let tag_window = json!({
        "type": "object",
        "properties": {
            "owner": {"type": "string"},
            "repo": {"type": "string"},
            "from_tag": {"type": "string"},
            "to_tag": {"type": "string"}
        },
        "required": ["from_tag", "to_tag"]
    });

    for (id, desc, params) in [
        (
            "github_recent_activity",
            "Fetch recent GitHub commits, pull requests, and issues.",
            repo_window.clone(),
        ),
        (
            "gitlab_recent_activity",
            "Fetch recent GitLab commits, merge requests, and issues.",
            repo_window.clone(),
        ),
        (
            "github_merged_prs",
            "Fetch GitHub pull requests merged between two tags.",
            tag_window.clone(),
        ),
        (
            "gitlab_merged_prs",
            "Fetch GitLab merge requests merged between two tags.",
            tag_window.clone(),
        ),
        (
            "github_pipelines",
            "Fetch GitHub pipeline/check information for a repository.",
            repo_window.clone(),
        ),
        (
            "gitlab_pipelines",
            "Fetch GitLab pipeline information for a repository.",
            repo_window.clone(),
        ),
    ] {
        out.insert(
            id.into(),
            def_from_executor(id, desc, params, integration.clone()),
        );
    }

    // --- Code / workspace tools ---
    if let Some(code) = code {
        let list_params = json!({"type":"object","properties":{}});
        out.insert(
            "list_repos".into(),
            def_from_executor(
                "list_repos",
                "List configured source repositories available to this tenant.",
                list_params,
                code.clone(),
            ),
        );
        out.insert(
            "code_search".into(),
            def_from_executor(
                "code_search",
                "Search repository contents with ripgrep-style text search.",
                json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "query": {"type": "string"},
                        "top_k": {"type": "integer", "minimum": 1, "maximum": 100}
                    },
                    "required": ["repo", "query"]
                }),
                code.clone(),
            ),
        );
        out.insert(
            "read_file".into(),
            def_from_executor(
                "read_file",
                "Read a file from a configured repository clone.",
                json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "path": {"type": "string"},
                        "max_bytes": {"type": "integer", "minimum": 1}
                    },
                    "required": ["repo", "path"]
                }),
                code.clone(),
            ),
        );
        out.insert(
            "list_tree".into(),
            def_from_executor(
                "list_tree",
                "List paths under a repository prefix.",
                json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "prefix": {"type": "string"},
                        "max_entries": {"type": "integer", "minimum": 1}
                    },
                    "required": ["repo"]
                }),
                code.clone(),
            ),
        );
    }

    // --- Artifact tools (ADR-0016) ---
    if let Some(art) = artifacts {
        let create_p = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "mime": { "type": "string" },
                "data": { "type": "string" },
                "labels": { "type": "object" }
            },
            "required": ["name", "data"]
        });
        out.insert(
            "create_artifact".into(),
            def_from_executor(
                "create_artifact",
                "Create a versioned named blob (text or base64) in tenant/task scope.",
                create_p,
                art.clone(),
            ),
        );
        out.insert(
            "append_artifact".into(),
            def_from_executor(
                "append_artifact",
                "Append bytes to the latest version and write a new version.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "data": { "type": "string" },
                        "mime": { "type": "string" }
                    },
                    "required": ["name", "data"]
                }),
                art.clone(),
            ),
        );
        out.insert(
            "list_artifacts".into(),
            def_from_executor(
                "list_artifacts",
                "List artifacts in the current scope; optional name prefix and label filter.",
                json!({
                    "type": "object",
                    "properties": {
                        "prefix": { "type": "string" },
                        "label": { "type": "string" },
                        "value": { "type": "string" }
                    }
                }),
                art.clone(),
            ),
        );
        out.insert(
            "load_artifact".into(),
            def_from_executor(
                "load_artifact",
                "Load an artifact: inline text/data if small, else file URI (presign or API proxy URL).",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "version": { "type": "integer" },
                        "max_inline_bytes": { "type": "integer" }
                    },
                    "required": ["name"]
                }),
                art.clone(),
            ),
        );
        out.insert(
            "artifact_meta".into(),
            def_from_executor(
                "artifact_meta",
                "Return JSON metadata (mime, size, labels) for a named version.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "version": { "type": "integer" }
                    },
                    "required": ["name"]
                }),
                art.clone(),
            ),
        );
        out.insert(
            "delete_artifact".into(),
            def_from_executor(
                "delete_artifact",
                "Delete one version, all versions, or use version: \"*\" for all.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "version": { "type": [ "integer", "string" ] }
                    },
                    "required": ["name"]
                }),
                art.clone(),
            ),
        );
        out.insert(
            "pin_artifact".into(),
            def_from_executor(
                "pin_artifact",
                "Add `pinned=true` to an artifact (retention sweeps ignore pinned).",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "version": { "type": "integer" }
                    },
                    "required": ["name"]
                }),
                art.clone(),
            ),
        );
    }
}

/// Peer skill tool: name encodes target agent; body matches `peer_*` descriptor schema.
pub struct PeerSkillToolDef {
    name: String,
    description: String,
    parameters: Value,
    agent_call: Arc<AgentCallToolExecutor>,
}

impl PeerSkillToolDef {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        agent_call: Arc<AgentCallToolExecutor>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            agent_call,
        }
    }
}

#[async_trait]
impl ToolDef for PeerSkillToolDef {
    fn id(&self) -> &str {
        self.name.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn input_schema(&self) -> &Value {
        &self.parameters
    }

    fn output_schema(&self) -> &Value {
        static OUT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        OUT.get_or_init(|| json!({"type": "object"}))
    }

    async fn invoke(&self, ctx: &AgentContext, input: &Value) -> Result<Value, OrkError> {
        self.agent_call
            .dispatch_peer_tool(ctx, &self.name, input)
            .await
    }
}
