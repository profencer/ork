//! Integration tests for the [`AgentCallToolExecutor`] that backs the LLM-facing
//! `agent_call` tool from ADR 0006. We exercise the tool in isolation (without
//! `LocalAgent`) by passing the caller context directly into
//! [`ToolExecutor::execute`] — the ADR 0011 replacement for the old
//! set/clear-caller-context seam.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures::stream;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, Message as AgentMessage, MessageId, Part, Role,
    TaskEvent as AgentEvent, TaskId, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, AgentId, CallerIdentity};
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::delegation_publisher::DelegationPublisher;
use ork_core::workflow::engine::ToolExecutor;
use ork_integrations::agent_call::AgentCallToolExecutor;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Stub agent that echoes the user prompt back as `"echo:<text>"`. Same shape as
/// the one in `ork-core/tests/common/mod.rs`; duplicated here because cross-
/// crate test fixtures are not exposed.
struct EchoAgent {
    id: AgentId,
    card: AgentCard,
}

impl EchoAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: card(id),
        }
    }
}

fn card(id: &str) -> AgentCard {
    AgentCard {
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
        Ok(Box::pin(stream::iter(events)))
    }
}

/// One captured `publish_request` call: target agent, child task id, payload bytes.
type RecordedRequest = (AgentId, TaskId, Vec<u8>);

#[derive(Default, Clone)]
struct RecordingPublisher {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[async_trait]
impl DelegationPublisher for RecordingPublisher {
    async fn publish_request(
        &self,
        target_agent: &AgentId,
        task_id: TaskId,
        payload: &[u8],
    ) -> Result<(), OrkError> {
        self.requests
            .lock()
            .await
            .push((target_agent.clone(), task_id, payload.to_vec()));
        Ok(())
    }

    async fn publish_cancel(&self, _task_id: TaskId) -> Result<(), OrkError> {
        Ok(())
    }
}

fn root_ctx(tenant: TenantId) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            // ADR-0020 + ADR-0021 policy gates: `agent_call` requires
            // `agent:<target>:delegate`, the composite tool plane requires
            // `tool:<name>:invoke` for the `peer_*` arm. Tests don't
            // exercise the gates themselves (separate suite); grant the
            // wildcards so the existing assertions still measure the SUT
            // (delegation outcome shape, publisher routing).
            scopes: vec!["agent:*:delegate".to_string(), "tool:*:invoke".to_string()],
            tenant_chain: vec![tenant],
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
        resource_id: None,
        thread_id: None,
    }
}

/// Build the registry + tool executor pair using the same `Arc::new_cyclic`
/// pattern as the production wiring in `ork-api`.
fn build_pair(
    publisher: Option<Arc<dyn DelegationPublisher>>,
) -> (Arc<AgentRegistry>, Arc<AgentCallToolExecutor>) {
    let echo: Arc<dyn Agent> = Arc::new(EchoAgent::new("echo")) as Arc<dyn Agent>;
    let executor: Arc<Mutex<Option<Arc<AgentCallToolExecutor>>>> = Arc::new(Mutex::new(None));
    let executor_capture = executor.clone();
    let registry = Arc::new_cyclic(|registry_weak: &Weak<AgentRegistry>| {
        let exec = Arc::new(AgentCallToolExecutor::new(
            registry_weak.clone(),
            publisher,
            None,
        ));
        executor_capture
            .try_lock()
            .expect("uncontended in test")
            .replace(exec);
        AgentRegistry::from_agents(vec![echo])
    });
    let exec = executor
        .try_lock()
        .expect("uncontended in test")
        .clone()
        .expect("executor was set inside Arc::new_cyclic");
    (registry, exec)
}

#[tokio::test]
async fn agent_call_sync_returns_reply_and_completed_status() {
    let tenant = TenantId::new();
    let (_registry, exec) = build_pair(None);

    let ctx = root_ctx(tenant);
    let result = exec
        .execute(
            &ctx,
            "agent_call",
            &serde_json::json!({
                "agent": "echo",
                "prompt": "hello peer",
                "await": true,
            }),
        )
        .await
        .expect("agent_call sync must succeed");

    assert_eq!(result["status"], "completed");
    assert!(
        result["task_id"].is_string(),
        "task_id must be present and stringy: {result}"
    );
    assert_eq!(result["reply"]["text"], "echo:hello peer");
}

#[tokio::test]
async fn agent_call_fire_and_forget_publishes_and_returns_submitted() {
    let tenant = TenantId::new();
    let recorder = RecordingPublisher::default();
    let publisher: Arc<dyn DelegationPublisher> = Arc::new(recorder.clone());
    let (_registry, exec) = build_pair(Some(publisher));

    let ctx = root_ctx(tenant);
    let result = exec
        .execute(
            &ctx,
            "agent_call",
            &serde_json::json!({
                "agent": "echo",
                "prompt": "go do thing",
                "await": false,
            }),
        )
        .await
        .expect("agent_call fire-and-forget must succeed");

    assert_eq!(result["status"], "submitted");

    let requests = recorder.requests.lock().await;
    assert_eq!(requests.len(), 1, "expected exactly one publish");
    let (target, task_id, payload) = &requests[0];
    assert_eq!(target.as_str(), "echo");
    assert_eq!(task_id.to_string(), result["task_id"].as_str().unwrap());
    let payload_json: serde_json::Value =
        serde_json::from_slice(payload).expect("publisher payload is JSON-RPC");
    assert_eq!(payload_json["method"], "message/send");
}

#[tokio::test]
async fn agent_call_accepts_caller_context_directly() {
    let tenant = TenantId::new();
    let (_registry, exec) = build_pair(None);

    let result = exec
        .execute(
            &root_ctx(tenant),
            "agent_call",
            &serde_json::json!({
                "agent": "echo",
                "prompt": "x",
                "await": true,
            }),
        )
        .await
        .expect("direct caller context should be enough for agent_call");
    assert_eq!(result["status"], "completed");
}

#[tokio::test]
async fn agent_call_unknown_tool_name_is_rejected() {
    let tenant = TenantId::new();
    let (_registry, exec) = build_pair(None);
    let ctx = root_ctx(tenant);

    let res = exec
        .execute(&ctx, "not_agent_call", &serde_json::json!({}))
        .await;
    match res {
        Ok(v) => panic!("expected error for wrong tool name; got {v}"),
        Err(OrkError::Integration(_)) => {}
        Err(other) => panic!("expected Integration error, got {other:?}"),
    }
}

// -- peer_<agent_id>_<skill_id> dispatch (ADR 0006 §`LLM tool surface`) ----

/// EchoAgent variant with a card whose `name` differs from the registry id and
/// whose skill has a non-default id. Used to pin both halves of Bug B:
///  - the `CompositeToolExecutor::peer_*` arm desugars through
///    `dispatch_peer_tool` instead of falling into the integration arm;
///  - the registry's reverse lookup keys off the registry id, not card.name.
struct CustomEchoAgent {
    id: AgentId,
    card: AgentCard,
}

impl CustomEchoAgent {
    fn new(id: &str, card_name: &str, skill_id: &str) -> Self {
        let mut c = card(id);
        c.name = card_name.to_string();
        c.skills[0].id = skill_id.to_string();
        c.skills[0].name = "doer".into();
        Self {
            id: id.into(),
            card: c,
        }
    }
}

#[async_trait]
impl Agent for CustomEchoAgent {
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
        // Reuse EchoAgent's body verbatim by delegating.
        EchoAgent {
            id: self.id.clone(),
            card: self.card.clone(),
        }
        .send_stream(ctx, msg)
        .await
    }
}

#[tokio::test]
async fn composite_dispatches_peer_tool_via_agent_call_under_casing_mismatch() {
    use ork_integrations::tool_plane::ToolPlaneExecutor;

    let tenant = TenantId::new();

    // Registry: id="synthesizer" (lowercase, like the real demo) with a card
    // named "Synthesizer" and a skill id "synth-default".
    let synth: Arc<dyn Agent> = Arc::new(CustomEchoAgent::new(
        "synthesizer",
        "Synthesizer",
        "synth-default",
    ));
    let executor: Arc<Mutex<Option<Arc<AgentCallToolExecutor>>>> = Arc::new(Mutex::new(None));
    let executor_capture = executor.clone();
    let _registry = Arc::new_cyclic(|registry_weak: &Weak<AgentRegistry>| {
        let exec = Arc::new(AgentCallToolExecutor::new(
            registry_weak.clone(),
            None,
            None,
        ));
        executor_capture
            .try_lock()
            .expect("uncontended in test")
            .replace(exec);
        AgentRegistry::from_agents(vec![synth])
    });
    let agent_call = executor
        .try_lock()
        .expect("uncontended in test")
        .clone()
        .expect("executor was set inside Arc::new_cyclic");

    let composite = ToolPlaneExecutor::new(Arc::new(HashMap::new()), Some(agent_call), None);

    let ctx = root_ctx(tenant);
    let result = composite
        .execute(
            &ctx,
            // The catalog advertises the tool under the registry id, NOT the
            // card display name. Pre-fix this name didn't exist in the catalog
            // (it produced `peer_Synthesizer_*`) and the executor didn't know
            // how to dispatch any `peer_*` name anyway.
            "peer_synthesizer_synth-default",
            &serde_json::json!({"prompt": "do the thing"}),
        )
        .await
        .expect("composite must dispatch peer tool through the agent_call arm");

    assert_eq!(result["status"], "completed");
    assert_eq!(result["reply"]["text"], "echo:do the thing");
}

#[tokio::test]
async fn composite_peer_tool_unknown_returns_clear_error() {
    use ork_integrations::tool_plane::ToolPlaneExecutor;

    let tenant = TenantId::new();
    let (_registry, exec) = build_pair(None);
    let composite = ToolPlaneExecutor::new(Arc::new(HashMap::new()), Some(exec), None);

    let err = composite
        .execute(
            &root_ctx(tenant),
            "peer_nope_does-not-exist",
            &serde_json::json!({"prompt": "x"}),
        )
        .await
        .unwrap_err();
    match err {
        OrkError::Integration(msg) => {
            assert!(
                msg.contains("unknown peer tool"),
                "error message should call out the unknown peer tool, got `{msg}`"
            );
        }
        other => panic!("expected Integration error, got {other:?}"),
    }
}

#[tokio::test]
async fn composite_peer_tool_without_agent_call_returns_explicit_error() {
    use ork_integrations::tool_plane::ToolPlaneExecutor;

    let tenant = TenantId::new();
    let composite = ToolPlaneExecutor::new(Arc::new(HashMap::new()), None, None);

    let err = composite
        .execute(
            &root_ctx(tenant),
            "peer_synthesizer_default",
            &serde_json::json!({"prompt": "x"}),
        )
        .await
        .unwrap_err();
    match err {
        OrkError::Integration(msg) => {
            assert!(
                msg.contains("peer tool"),
                "error message should mention the peer tool name, got `{msg}`"
            );
            assert!(
                msg.contains("ADR-0006"),
                "error must point operators at ADR-0006, got `{msg}`"
            );
        }
        other => panic!("expected Integration error, got {other:?}"),
    }
}

// =============================================================================
// ADR-0021 §`Decision points` step 2 — `agent:<target>:delegate` gate +
// `tenant:cross_delegate` for remote agents.
// =============================================================================

/// `agent_call` denies the call when the caller does not carry
/// `agent:<target>:delegate`. Pinned so a future refactor cannot silently
/// drop the gate.
#[tokio::test]
async fn agent_call_denied_without_delegate_scope() {
    let tenant = TenantId::new();
    let (_registry, exec) = build_pair(None);

    // Strip the wildcard delegate scope set by `root_ctx`.
    let mut ctx = root_ctx(tenant);
    ctx.caller.scopes = vec!["tool:*:invoke".into()];

    let err = exec
        .execute(
            &ctx,
            "agent_call",
            &serde_json::json!({"agent": "echo", "prompt": "hi"}),
        )
        .await
        .unwrap_err();

    match err {
        OrkError::Forbidden(msg) => {
            assert!(
                msg.contains("agent:echo:delegate"),
                "deny message must surface the missing scope, got `{msg}`"
            );
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

/// Cross-tenant delegation (remote agent in the registry) requires both
/// `agent:<target>:delegate` AND `tenant:cross_delegate`. Without the
/// latter, the call is denied even when the caller holds the wildcard
/// delegate scope.
#[tokio::test]
async fn agent_call_denied_for_remote_agent_without_cross_delegate() {
    use ork_core::agent_registry::{RemoteAgentEntry, TransportHint};
    use std::time::{Duration, Instant};
    use url::Url;

    let tenant = TenantId::new();
    // Registry seeded with `echo` as a *local* agent, plus `vendor` as a
    // *remote* (TTL-cached) entry whose stub agent is the same EchoAgent
    // shape. Only the locality bit matters for the cross-tenant gate.
    let echo: Arc<dyn Agent> = Arc::new(EchoAgent::new("echo")) as Arc<dyn Agent>;
    let vendor_card = card("vendor");
    let vendor: Arc<dyn Agent> = Arc::new(EchoAgent::new("vendor")) as Arc<dyn Agent>;

    let executor: Arc<Mutex<Option<Arc<AgentCallToolExecutor>>>> = Arc::new(Mutex::new(None));
    let executor_capture = executor.clone();
    let registry = Arc::new_cyclic(|registry_weak: &Weak<AgentRegistry>| {
        let exec = Arc::new(AgentCallToolExecutor::new(
            registry_weak.clone(),
            None,
            None,
        ));
        executor_capture
            .try_lock()
            .expect("uncontended in test")
            .replace(exec);
        AgentRegistry::from_agents(vec![echo])
    });
    registry
        .upsert_remote_with_agent(
            "vendor".to_string(),
            RemoteAgentEntry {
                card: vendor_card,
                last_seen: Instant::now(),
                ttl: Duration::from_secs(60),
                transport_hint: TransportHint::HttpOnly {
                    url: Url::parse("https://vendor.example.com/a2a").unwrap(),
                },
                agent: None,
            },
            vendor,
        )
        .await;
    let exec = executor
        .try_lock()
        .expect("uncontended in test")
        .clone()
        .expect("executor set inside Arc::new_cyclic");

    // Caller has the delegate wildcard but NOT `tenant:cross_delegate`.
    let mut ctx = root_ctx(tenant);
    ctx.caller.scopes = vec!["agent:*:delegate".into(), "tool:*:invoke".into()];

    let err = exec
        .execute(
            &ctx,
            "agent_call",
            &serde_json::json!({"agent": "vendor", "prompt": "hi"}),
        )
        .await
        .unwrap_err();
    match err {
        OrkError::Forbidden(msg) => {
            assert!(
                msg.contains("tenant:cross_delegate"),
                "remote-agent denial must surface `tenant:cross_delegate`, got `{msg}`"
            );
            assert!(
                msg.contains("vendor"),
                "deny message should name the target agent, got `{msg}`"
            );
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }

    // With `tenant:cross_delegate` granted, the call gets past the gate
    // (and reaches `execute_one_shot_delegation`, which talks to the
    // EchoAgent stub).
    ctx.caller.scopes = vec![
        "agent:*:delegate".into(),
        "tool:*:invoke".into(),
        "tenant:cross_delegate".into(),
    ];
    let ok = exec
        .execute(
            &ctx,
            "agent_call",
            &serde_json::json!({"agent": "vendor", "prompt": "hi"}),
        )
        .await
        .expect("with cross-delegate scope, call should succeed");
    assert_eq!(ok["status"], "completed");
}
