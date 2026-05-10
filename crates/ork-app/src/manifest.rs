//! Serializable application manifest (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::inner::OrkAppInner;
use crate::types::{Environment, McpTransportStub, ObservabilityConfig, ServerConfig};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub description: String,
    pub card_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkflowSummary {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolSummary {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpServerSummary {
    pub id: String,
    pub transport: McpTransportStub,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MemorySummary {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VectorStoreSummary {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScorerSummary {
    pub target: serde_json::Value,
    pub scorer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorer_label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerSummary {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
    pub auth_mode: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AppManifest {
    pub environment: Environment,
    pub agents: Vec<AgentSummary>,
    pub workflows: Vec<WorkflowSummary>,
    pub tools: Vec<ToolSummary>,
    pub mcp_servers: Vec<McpServerSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<VectorStoreSummary>,
    pub scorers: Vec<ScorerSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observability: Option<ObservabilityConfig>,
    pub server: ServerSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_context_schema: Option<serde_json::Value>,
    pub built_at: DateTime<Utc>,
    pub ork_version: String,
}

fn server_summary(cfg: &ServerConfig) -> ServerSummary {
    ServerSummary {
        host: cfg.host.clone(),
        port: cfg.port,
        tls_enabled: cfg.tls.is_some(),
        auth_mode: cfg.auth.as_ref().map(|a| a.mode.clone()),
    }
}

pub(crate) fn build_manifest(inner: &OrkAppInner) -> AppManifest {
    let mut agent_ids: Vec<_> = inner.agents.keys().cloned().collect();
    agent_ids.sort();
    let agents: Vec<AgentSummary> = agent_ids
        .into_iter()
        .filter_map(|id| {
            inner.agents.get(&id).map(|a| AgentSummary {
                id: id.clone(),
                description: a.card().description.clone(),
                card_name: a.card().name.clone(),
            })
        })
        .collect();

    let mut wf_ids: Vec<_> = inner.workflows.keys().cloned().collect();
    wf_ids.sort();
    let workflows: Vec<WorkflowSummary> = wf_ids
        .into_iter()
        .filter_map(|id| {
            inner.workflows.get(&id).map(|w| WorkflowSummary {
                id: id.clone(),
                description: w.description().into(),
            })
        })
        .collect();

    let mut tool_ids: Vec<_> = inner.tools.keys().cloned().collect();
    tool_ids.sort();
    let tools: Vec<ToolSummary> = tool_ids
        .into_iter()
        .filter_map(|id| {
            inner.tools.get(&id).map(|t| ToolSummary {
                id: id.clone(),
                description: t.description().into(),
            })
        })
        .collect();

    let mut mcp: Vec<McpServerSummary> = inner
        .mcp_servers
        .iter()
        .map(|(id, spec)| McpServerSummary {
            id: id.clone(),
            transport: spec.transport.clone(),
        })
        .collect();
    mcp.sort_by(|a, b| a.id.cmp(&b.id));

    let memory = inner.memory.as_ref().map(|m| MemorySummary {
        name: m.name().into(),
    });
    let vectors = inner.vectors.as_ref().map(|v| VectorStoreSummary {
        name: v.name().into(),
    });

    let scorers: Vec<ScorerSummary> = inner
        .scorers
        .iter()
        .map(|b| ScorerSummary {
            target: serde_json::to_value(&b.target)
                .expect("ScorerTarget must serialize into manifest JSON snapshot"),
            scorer_id: b.spec.scorer().id().to_string(),
            scorer_label: None,
        })
        .collect();

    AppManifest {
        environment: inner.environment.clone(),
        agents,
        workflows,
        tools,
        mcp_servers: mcp,
        memory,
        vectors,
        scorers,
        observability: inner.observability.clone(),
        server: server_summary(&inner.server_config),
        request_context_schema: inner.request_context_schema.clone(),
        built_at: inner.built_at,
        ork_version: inner.ork_version.clone(),
    }
}
