use std::sync::Arc;

use ork_common::config::AppConfig;
use ork_core::agent_registry::AgentRegistry;
use ork_core::embeds::{EmbedLimits, EmbedRegistry};
use ork_core::ports::a2a_push_repo::A2aPushConfigRepository;
use ork_core::ports::a2a_task_repo::A2aTaskRepository;
use ork_core::ports::artifact_meta_repo::ArtifactMetaRepo;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::remote_agent_builder::RemoteAgentBuilder;
use ork_core::services::tenant::TenantService;
use ork_core::services::workflow::WorkflowService;
use ork_core::workflow::engine::WorkflowEngine;
use ork_eventing::EventingClient;
use ork_push::{JwksProvider, PushService};

use crate::sse_buffer::SseBuffer;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub tenant_service: Arc<TenantService>,
    pub workflow_service: Arc<WorkflowService>,
    pub agent_registry: Arc<AgentRegistry>,
    pub engine: Arc<WorkflowEngine>,
    /// Hybrid Kong/Kafka transport client (ADR-0004). Cheap to clone (inner `Arc`s).
    pub eventing: EventingClient,
    /// Shared `A2aRemoteAgent` factory (ADR-0007). Reused by the static-config loader,
    /// the discovery subscriber, the workflow inline-card overlay, and ADR-0014 plugin
    /// code so every `A2aRemoteAgent` in this process shares one HTTP client, Redis
    /// card cache, retry policy, and Kafka short-circuit publisher.
    pub remote_builder: Arc<dyn RemoteAgentBuilder>,
    /// ADR-0008 §`Persistence`: A2A task + message log shared by every JSON-RPC
    /// handler. Same instance the workflow engine uses for delegated runs so the
    /// inbound and outbound A2A surfaces stay coherent.
    pub a2a_task_repo: Arc<dyn A2aTaskRepository>,
    /// ADR-0009 (pulled forward by ADR-0008): per-task push notification config
    /// store; backs `tasks/pushNotificationConfig/{set,get}`.
    pub a2a_push_repo: Arc<dyn A2aPushConfigRepository>,
    /// ADR-0008 §`SSE bridge`: replay buffer used by the `GET .../stream/{task_id}`
    /// resume path. Production wires the Redis variant; dev/test wires the
    /// in-memory variant when Redis is unreachable.
    pub sse_buffer: Arc<dyn SseBuffer>,
    /// ADR-0009 push notifications: outbox publisher invoked from the JSON-RPC
    /// dispatcher whenever a task transitions to a terminal state. Cheap to
    /// clone — wraps the shared eventing producer.
    pub push_service: Arc<PushService>,
    /// ADR-0009 push notifications: JWKS provider that backs both
    /// `/.well-known/jwks.json` and the worker's outbound JWS signer. Cached
    /// snapshot is shared so a single rotation propagates to every site.
    pub jwks_provider: Arc<JwksProvider>,
    /// ADR-0015: late-phase `«type:…»` resolution on the A2A SSE stream.
    pub embed_registry: Arc<EmbedRegistry>,
    pub embed_limits: EmbedLimits,
    /// ADR-0016: optional blob store + index; both set when [`AppConfig::artifacts`]
    /// is enabled. Used for A2A `Part::File` rewrites, proxy GET, and tool/embed paths.
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    pub artifact_meta: Option<Arc<dyn ArtifactMetaRepo>>,
    /// e.g. `https://api.example` — no path; used for public [`FileRef::Uri`].
    pub artifact_public_base: String,
}
