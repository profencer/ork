//! ADR-0051: native tool parameter schemas stay byte-stable vs the pre-DSL hand-authored JSON.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, Message as AgentMessage, MessageId, Part, Role,
    TaskEvent as AgentEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::artifact_meta_repo::{ArtifactMetaRepo, ArtifactRow};
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use ork_core::ports::tool_def::ToolDef;
use ork_integrations::agent_call::AgentCallToolExecutor;
use ork_integrations::artifact_tools::ArtifactToolExecutor;
use ork_integrations::code_tools::CodeToolExecutor;
use ork_integrations::native_tool_defs::extend_native_tool_map;
use ork_integrations::tools::IntegrationToolExecutor;
use ork_integrations::workspace::GitRepoWorkspace;
use serde_json::Value;

struct EchoAgent {
    id: AgentId,
    card: AgentCard,
}

impl EchoAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: AgentCard {
                name: id.to_string(),
                description: "test stub".into(),
                version: "0.0.0".into(),
                url: None,
                provider: None,
                capabilities: AgentCapabilities {
                    streaming: true,
                    push_notifications: false,
                    state_transition_history: false,
                },
                default_input_modes: vec!["text/plain".into()],
                default_output_modes: vec!["text/plain".into()],
                skills: vec![AgentSkill {
                    id: "stub".into(),
                    name: "stub".into(),
                    description: "stub".into(),
                    tags: vec![],
                    examples: vec![],
                    input_modes: None,
                    output_modes: None,
                }],
                security_schemes: None,
                security: None,
                extensions: None,
            },
        }
    }
}

#[async_trait]
impl Agent for EchoAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn card(&self) -> &AgentCard {
        &self.card
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let mut text = String::new();
        for p in &msg.parts {
            if let Part::Text { text: t, .. } = p {
                text.push_str(t);
            }
        }
        let task_id = ctx.task_id;
        let reply = AgentMessage {
            role: Role::Agent,
            parts: vec![Part::Text {
                text: format!("echo:{text}"),
                metadata: None,
            }],
            message_id: MessageId::new(),
            task_id: Some(task_id),
            context_id: ctx.context_id,
            metadata: None,
        };
        let events: Vec<Result<AgentEvent, OrkError>> = vec![
            Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                },
                is_final: false,
            })),
            Ok(AgentEvent::Message(reply)),
            Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                },
                is_final: true,
            })),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

fn build_registry_and_agent_call() -> Arc<AgentCallToolExecutor> {
    let echo: Arc<dyn Agent> = Arc::new(EchoAgent::new("echo")) as Arc<dyn Agent>;
    let slot: Arc<Mutex<Option<Arc<AgentCallToolExecutor>>>> = Arc::new(Mutex::new(None));
    let slot_c = slot.clone();
    let _registry = Arc::new_cyclic(|registry_weak: &Weak<AgentRegistry>| {
        let exec = Arc::new(AgentCallToolExecutor::new(
            registry_weak.clone(),
            None,
            None,
        ));
        *slot_c.try_lock().expect("lock") = Some(exec);
        AgentRegistry::from_agents(vec![echo])
    });
    slot.try_lock()
        .expect("lock")
        .clone()
        .expect("executor set")
}

struct MemStore;

#[async_trait]
impl ArtifactStore for MemStore {
    fn scheme(&self) -> &'static str {
        "mem"
    }

    async fn put(
        &self,
        _scope: &ArtifactScope,
        _name: &str,
        _body: ArtifactBody,
        _meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        unreachable!("parity test does not execute artifact tools")
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        let _ = r#ref;
        unreachable!()
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        let _ = r#ref;
        unreachable!()
    }

    async fn list(
        &self,
        _scope: &ArtifactScope,
        _prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        Ok(vec![])
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }
}

struct MemMeta;

#[async_trait]
impl ArtifactMetaRepo for MemMeta {
    async fn upsert(&self, _row: &ArtifactRow) -> Result<(), OrkError> {
        Ok(())
    }

    async fn latest_version(
        &self,
        _tenant: TenantId,
        _context: Option<ork_a2a::ContextId>,
        _name: &str,
    ) -> Result<Option<u32>, OrkError> {
        Ok(None)
    }

    async fn list(
        &self,
        _scope: &ArtifactScope,
        _prefix: Option<&str>,
        _label_eq: Option<(&str, &str)>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        Ok(vec![])
    }

    async fn delete_version(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }

    async fn delete_all_versions(
        &self,
        _scope: &ArtifactScope,
        _name: &str,
    ) -> Result<u32, OrkError> {
        Ok(0)
    }

    async fn eligible_for_sweep(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
        _default_days: u32,
        _task_days: u32,
    ) -> Result<Vec<ArtifactRef>, OrkError> {
        Ok(vec![])
    }

    async fn add_label(&self, r#ref: &ArtifactRef, _k: &str, _v: &str) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }
}

fn build_full_native_map() -> HashMap<String, Arc<dyn ToolDef>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Arc::new(GitRepoWorkspace::new(dir.path().to_path_buf(), 1, vec![]));
    let code = Arc::new(CodeToolExecutor::new(workspace));
    let art = Arc::new(ArtifactToolExecutor::new(
        Arc::new(MemStore),
        Arc::new(MemMeta),
        None,
    ));
    let integration = Arc::new(IntegrationToolExecutor::new());
    let agent_call = build_registry_and_agent_call();

    let mut m = HashMap::new();
    extend_native_tool_map(&mut m, integration, Some(code), Some(art), Some(agent_call));
    m
}

#[test]
fn native_tool_input_schemas_match_legacy_fixture() {
    let raw = include_str!("fixtures/legacy_tool_parameters.json");
    let fixture_map: HashMap<String, Value> = serde_json::from_str(raw).expect("parse fixture");

    let built = build_full_native_map();

    for (name, expected) in &fixture_map {
        if name == "peer_prompt_data" {
            continue;
        }
        let def = built
            .get(name)
            .unwrap_or_else(|| panic!("tool `{name}` missing from native map"));
        assert_eq!(
            def.input_schema(),
            expected,
            "tool `{name}` input_schema drift — update native_tool_defs or fixture deliberately"
        );
        let built_bytes = serde_json::to_vec(def.input_schema()).unwrap();
        let expected_bytes = serde_json::to_vec(expected).unwrap();
        assert_eq!(
            built_bytes, expected_bytes,
            "tool `{name}` schema JSON bytes must match legacy fixture exactly"
        );
    }

    for name in built.keys() {
        if name.starts_with("peer_") {
            continue;
        }
        assert!(
            fixture_map.contains_key(name.as_str()),
            "tool `{name}` present in native map but missing from legacy fixture — extend fixture"
        );
    }
}
