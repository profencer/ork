//! Shared test fixtures for the `ork-api` integration tests added in ADR-0008.
//!
//! Builds an `AppState` whose dependencies are all in-memory so the JSON-RPC
//! dispatcher and SSE bridge tests can run without a Postgres or Redis service.
//! The Postgres-backed repos in `ork-persistence` are exercised separately by
//! `crates/ork-persistence/tests/`.
//!
//! Pieces:
//!
//! - [`TestAgent`]: deterministic in-memory `Agent` that emits a single
//!   `agent_text("ack: …")` reply.
//! - [`InMemoryA2aTaskRepo`] / [`InMemoryA2aPushRepo`]: minimal port impls that
//!   cover the surface the dispatcher uses.
//! - [`StubTenantRepository`] / [`StubRemoteAgentBuilder`]: trivial fillers so
//!   `AppState` can be constructed.
//! - [`test_state`]: assemble all of the above into a ready `AppState`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use futures::stream;
use ork_a2a::{AgentCard, JsonRpcRequest, Message, Part, TaskEvent, TaskId};
use ork_api::middleware::AuthContext;
use ork_api::sse_buffer::{InMemorySseBuffer, SseBuffer};
use ork_api::state::AppState;
use ork_common::config::AppConfig;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::agent::AgentConfig;
use ork_core::models::tenant::{CreateTenantRequest, Tenant, UpdateTenantSettingsRequest};
use ork_core::ports::a2a_push_dead_letter_repo::{
    A2aPushDeadLetterRepository, A2aPushDeadLetterRow,
};
use ork_core::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use ork_core::ports::a2a_signing_key_repo::{A2aSigningKeyRepository, A2aSigningKeyRow};
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::remote_agent_builder::RemoteAgentBuilder;
use ork_core::ports::repository::{TenantRepository, WorkflowRepository};
use ork_core::services::tenant::TenantService;
use ork_core::services::workflow::WorkflowService;
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::engine::WorkflowEngine;
use ork_eventing::EventingClient;
use ork_push::worker::{PushDeliveryWorker, WorkerConfig};
use ork_push::{JwksProvider, PushService, encryption, signing::RotationPolicy};
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

// =============================================================================
// In-memory `Agent` impl
// =============================================================================

pub struct TestAgent {
    pub id: AgentId,
    pub card: AgentCard,
}

#[async_trait]
impl Agent for TestAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        _ctx: AgentContext,
        msg: Message,
    ) -> Result<AgentEventStream, OrkError> {
        let preview = msg
            .parts
            .iter()
            .find_map(|p| match p {
                Part::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let reply = Message::agent_text(format!("ack: {preview}"));
        let stream = stream::iter(vec![Ok(TaskEvent::Message(reply))]);
        Ok(Box::pin(stream))
    }
}

pub fn enriched_ctx() -> CardEnrichmentContext {
    CardEnrichmentContext {
        public_base_url: Some(Url::parse("https://api.example.com/").unwrap()),
        provider_organization: Some("Example".into()),
        devportal_url: Some(Url::parse("https://devportal.example.com/").unwrap()),
        namespace: "ork.a2a.v1".into(),
        include_tenant_required_ext: false,
        tenant_header: "X-Tenant-Id".into(),
    }
}

pub fn test_agent(id: &str) -> Arc<dyn Agent> {
    let cfg = AgentConfig {
        id: id.into(),
        name: format!("{id} agent"),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec![],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 100,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    };
    Arc::new(TestAgent {
        id: id.into(),
        card: build_local_card(&cfg, &enriched_ctx()),
    })
}

// =============================================================================
// In-memory port implementations
// =============================================================================

#[derive(Default)]
pub struct InMemoryA2aTaskRepo {
    tasks: Mutex<HashMap<TaskId, A2aTaskRow>>,
    messages: Mutex<HashMap<TaskId, Vec<A2aMessageRow>>>,
}

impl InMemoryA2aTaskRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl A2aTaskRepository for InMemoryA2aTaskRepo {
    async fn create_task(&self, row: &A2aTaskRow) -> Result<(), OrkError> {
        self.tasks.lock().await.insert(row.id, row.clone());
        Ok(())
    }
    async fn update_state(
        &self,
        tenant_id: TenantId,
        id: TaskId,
        state: ork_a2a::TaskState,
    ) -> Result<(), OrkError> {
        let mut g = self.tasks.lock().await;
        if let Some(row) = g.get_mut(&id)
            && row.tenant_id == tenant_id
        {
            row.state = state;
            row.updated_at = chrono::Utc::now();
            if matches!(
                state,
                ork_a2a::TaskState::Completed
                    | ork_a2a::TaskState::Failed
                    | ork_a2a::TaskState::Canceled
                    | ork_a2a::TaskState::Rejected
            ) {
                row.completed_at = Some(chrono::Utc::now());
            }
        }
        Ok(())
    }
    async fn get_task(
        &self,
        tenant_id: TenantId,
        id: TaskId,
    ) -> Result<Option<A2aTaskRow>, OrkError> {
        Ok(self
            .tasks
            .lock()
            .await
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned())
    }
    async fn append_message(&self, row: &A2aMessageRow) -> Result<(), OrkError> {
        self.messages
            .lock()
            .await
            .entry(row.task_id)
            .or_default()
            .push(row.clone());
        Ok(())
    }
    async fn list_messages(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
        history_length: Option<u32>,
    ) -> Result<Vec<A2aMessageRow>, OrkError> {
        let task_belongs = self
            .tasks
            .lock()
            .await
            .get(&task_id)
            .map(|t| t.tenant_id == tenant_id)
            .unwrap_or(false);
        if !task_belongs {
            return Ok(vec![]);
        }
        let mut rows = self
            .messages
            .lock()
            .await
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        if let Some(n) = history_length {
            rows.truncate(n as usize);
        }
        Ok(rows)
    }
    async fn list_tasks_in_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aTaskRow>, OrkError> {
        let mut rows: Vec<_> = self
            .tasks
            .lock()
            .await
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        rows.truncate(limit as usize);
        Ok(rows)
    }
}

#[derive(Default)]
pub struct InMemoryA2aPushRepo {
    rows: Mutex<HashMap<TaskId, A2aPushConfigRow>>,
}

impl InMemoryA2aPushRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl A2aPushConfigRepository for InMemoryA2aPushRepo {
    async fn upsert(&self, row: &A2aPushConfigRow) -> Result<(), OrkError> {
        self.rows.lock().await.insert(row.task_id, row.clone());
        Ok(())
    }
    async fn get(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Option<A2aPushConfigRow>, OrkError> {
        Ok(self
            .rows
            .lock()
            .await
            .get(&task_id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned())
    }
    async fn list_for_task(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Vec<A2aPushConfigRow>, OrkError> {
        Ok(self
            .rows
            .lock()
            .await
            .values()
            .filter(|r| r.task_id == task_id && r.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn count_active_for_tenant(&self, tenant_id: TenantId) -> Result<u64, OrkError> {
        Ok(self
            .rows
            .lock()
            .await
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .count() as u64)
    }
    async fn delete_terminal_after(
        &self,
        _older_than: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, OrkError> {
        // In-memory tests don't have a tasks table to join against; the unit
        // tests for the janitor live next to its Postgres impl. Tests that
        // need to assert the behaviour can preload a handcrafted in-memory
        // implementation.
        Ok(0)
    }
}

#[derive(Default)]
pub struct InMemorySigningKeyRepo {
    rows: Mutex<Vec<A2aSigningKeyRow>>,
}

impl InMemorySigningKeyRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl A2aSigningKeyRepository for InMemorySigningKeyRepo {
    async fn insert(&self, row: &A2aSigningKeyRow) -> Result<(), OrkError> {
        self.rows.lock().await.push(row.clone());
        Ok(())
    }
    async fn list_active(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<A2aSigningKeyRow>, OrkError> {
        let mut out: Vec<_> = self
            .rows
            .lock()
            .await
            .iter()
            .filter(|r| r.expires_at > now)
            .cloned()
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(out)
    }
    async fn mark_rotated(
        &self,
        id: Uuid,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), OrkError> {
        for r in self.rows.lock().await.iter_mut() {
            if r.id == id {
                r.rotated_out_at = Some(at);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct InMemoryDeadLetterRepo {
    rows: Mutex<Vec<A2aPushDeadLetterRow>>,
}

impl InMemoryDeadLetterRepo {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn snapshot(&self) -> Vec<A2aPushDeadLetterRow> {
        self.rows.lock().await.clone()
    }
}

#[async_trait]
impl A2aPushDeadLetterRepository for InMemoryDeadLetterRepo {
    async fn insert(&self, row: &A2aPushDeadLetterRow) -> Result<(), OrkError> {
        self.rows.lock().await.push(row.clone());
        Ok(())
    }
    async fn list_for_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aPushDeadLetterRow>, OrkError> {
        let mut out: Vec<_> = self
            .rows
            .lock()
            .await
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.failed_at.cmp(&a.failed_at));
        out.truncate(limit as usize);
        Ok(out)
    }
}

// =============================================================================
// Stub services for AppState scaffolding
// =============================================================================

pub struct StubTenantRepository;

#[async_trait]
impl TenantRepository for StubTenantRepository {
    async fn create(&self, _req: &CreateTenantRequest) -> Result<Tenant, OrkError> {
        Err(OrkError::Internal("stub".into()))
    }
    async fn get_by_id(&self, _id: TenantId) -> Result<Tenant, OrkError> {
        Err(OrkError::NotFound("stub".into()))
    }
    async fn get_by_slug(&self, _slug: &str) -> Result<Tenant, OrkError> {
        Err(OrkError::NotFound("stub".into()))
    }
    async fn list(&self) -> Result<Vec<Tenant>, OrkError> {
        Ok(vec![])
    }
    async fn update_settings(
        &self,
        _id: TenantId,
        _req: &UpdateTenantSettingsRequest,
    ) -> Result<Tenant, OrkError> {
        Err(OrkError::Internal("stub".into()))
    }
    async fn delete(&self, _id: TenantId) -> Result<(), OrkError> {
        Ok(())
    }
}

pub struct StubRemoteAgentBuilder;

#[async_trait]
impl RemoteAgentBuilder for StubRemoteAgentBuilder {
    async fn build(&self, _card: AgentCard) -> Result<Arc<dyn Agent>, OrkError> {
        Err(OrkError::Unsupported("stub remote builder".into()))
    }
}

// =============================================================================
// AppState builder
// =============================================================================

pub struct TestState {
    pub state: AppState,
    pub tenant_id: TenantId,
    pub task_repo: Arc<InMemoryA2aTaskRepo>,
    pub push_repo: Arc<InMemoryA2aPushRepo>,
    pub sse_buffer: Arc<InMemorySseBuffer>,
    pub eventing: EventingClient,
    pub signing_key_repo: Arc<InMemorySigningKeyRepo>,
    pub dead_letter_repo: Arc<InMemoryDeadLetterRepo>,
    pub jwks_provider: Arc<JwksProvider>,
    pub push_service: Arc<PushService>,
}

pub async fn test_state() -> TestState {
    test_state_with_agents(&["planner"]).await
}

pub async fn test_state_with_agent(agent_id: &str) -> TestState {
    test_state_with_agents(&[agent_id]).await
}

/// ADR-0009 §`Tests` — alias for the existing `test_state` builder so tests
/// added in the push-notifications PR can use the helper name the plan
/// references. Returns the same `TestState` (which now also exposes the
/// signing-key + dead-letter repos and the boot-time `JwksProvider`).
pub async fn test_state_with_push() -> TestState {
    test_state().await
}

/// Build (but don't spawn) a `PushDeliveryWorker` wired to the same
/// in-memory eventing client + push/dead-letter repos as the test state. The
/// caller owns the spawn so tests can plug in their own `CancellationToken`
/// and `WorkerConfig` (typically with sub-second `retry_intervals` for the
/// retry / dead-letter assertions).
#[must_use]
pub fn build_worker(t: &TestState, cfg: WorkerConfig) -> PushDeliveryWorker {
    PushDeliveryWorker::new(
        t.eventing.clone(),
        "ork.a2a.v1".into(),
        t.push_repo.clone() as Arc<dyn A2aPushConfigRepository>,
        t.dead_letter_repo.clone() as Arc<dyn A2aPushDeadLetterRepository>,
        t.jwks_provider.clone(),
        cfg,
    )
}

pub async fn test_state_with_agents(agent_ids: &[&str]) -> TestState {
    let tenant_id = TenantId::new();
    let agents: Vec<_> = agent_ids.iter().map(|id| test_agent(id)).collect();
    let registry = Arc::new(AgentRegistry::from_agents(agents));
    let task_repo = Arc::new(InMemoryA2aTaskRepo::new());
    let push_repo = Arc::new(InMemoryA2aPushRepo::new());
    let sse_buffer = Arc::new(InMemorySseBuffer::new(std::time::Duration::from_secs(60)));
    let eventing = EventingClient::in_memory();
    let workflow_repo: Arc<dyn WorkflowRepository> = Arc::new(NoopWorkflowRepository);
    let tenant_service = Arc::new(TenantService::new(Arc::new(StubTenantRepository)));
    let workflow_service = Arc::new(WorkflowService::new(workflow_repo.clone()));
    let embed_registry = Arc::new(ork_core::embeds::EmbedRegistry::with_builtins());
    let embed_limits = ork_core::embeds::EmbedLimits::default();
    let engine = Arc::new(
        WorkflowEngine::new(workflow_repo, registry.clone())
            .with_embeds(embed_registry.clone(), embed_limits.clone()),
    );
    let remote_builder: Arc<dyn RemoteAgentBuilder> = Arc::new(StubRemoteAgentBuilder);

    let signing_key_repo = Arc::new(InMemorySigningKeyRepo::new());
    let dead_letter_repo = Arc::new(InMemoryDeadLetterRepo::new());
    let kek = encryption::derive_kek("ork-api-tests-jwt-secret");
    let jwks_provider = JwksProvider::new(
        signing_key_repo.clone() as Arc<dyn A2aSigningKeyRepository>,
        kek,
        RotationPolicy::default(),
    )
    .await
    .expect("build JwksProvider for test fixtures");
    jwks_provider
        .ensure_at_least_one(chrono::Utc::now())
        .await
        .expect("ensure first signing key");
    let push_service = PushService::new(eventing.clone(), "ork.a2a.v1".into());

    let state = AppState {
        config: AppConfig::default(),
        tenant_service,
        workflow_service,
        agent_registry: registry,
        engine,
        eventing: eventing.clone(),
        remote_builder,
        a2a_task_repo: task_repo.clone() as Arc<dyn A2aTaskRepository>,
        a2a_push_repo: push_repo.clone() as Arc<dyn A2aPushConfigRepository>,
        sse_buffer: sse_buffer.clone() as Arc<dyn SseBuffer>,
        push_service: push_service.clone(),
        jwks_provider: jwks_provider.clone(),
        embed_registry,
        embed_limits,
        artifact_store: None,
        artifact_meta: None,
        artifact_public_base: "http://127.0.0.1:0".into(),
        webui_store: Arc::new(ork_webui::in_memory_store::InMemoryWebuiStore::new())
            as Arc<dyn ork_core::ports::webui_store::WebuiStore>,
    };

    TestState {
        state,
        tenant_id,
        task_repo,
        push_repo,
        sse_buffer,
        eventing,
        signing_key_repo,
        dead_letter_repo,
        jwks_provider,
        push_service,
    }
}

// =============================================================================
// HTTP test helpers
// =============================================================================

/// `AuthContext` extension for direct extension-injection (skips JWT decode).
pub fn auth_for(tenant_id: TenantId) -> AuthContext {
    auth_for_with_scopes(tenant_id, &[])
}

/// Same as [`auth_for`] but with an explicit scope list. ADR-0020 added
/// scope-gated routes; tests targeting those should use this so the JWT
/// shape mirrors what `auth_middleware` would have built from a real token.
pub fn auth_for_with_scopes(tenant_id: TenantId, scopes: &[&str]) -> AuthContext {
    AuthContext {
        tenant_id,
        user_id: "test-user".into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        tenant_chain: Vec::new(),
        trust_tier: ork_common::auth::TrustTier::default(),
        trust_class: ork_common::auth::TrustClass::default(),
        agent_id: None,
    }
}

pub async fn read_body(resp: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()))
}

pub fn jsonrpc_request(id: serde_json::Value, method: &str, params: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&JsonRpcRequest::<serde_json::Value> {
        jsonrpc: "2.0".into(),
        id: Some(id),
        method: method.into(),
        params: Some(params),
    })
    .unwrap()
}
