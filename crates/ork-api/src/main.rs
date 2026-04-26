use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use ork_api::{remote_agents, routes, state};
use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::ports::remote_agent_builder::RemoteAgentBuilder;
use ork_eventing::discovery::{CardProvider, DiscoveryPublisher, DiscoverySubscriber};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    let config = ork_common::config::AppConfig::load().unwrap_or_else(|_| {
        info!("using default configuration");
        ork_common::config::AppConfig::default()
    });

    let pool = ork_persistence::postgres::create_pool(
        &config.database.url,
        config.database.max_connections,
    )
    .await
    .context("failed to connect to database")?;

    let tenant_repo =
        Arc::new(ork_persistence::postgres::tenant_repo::PgTenantRepository::new(pool.clone()));
    let workflow_repo =
        Arc::new(ork_persistence::postgres::workflow_repo::PgWorkflowRepository::new(pool.clone()));
    let a2a_task_repo: Arc<dyn ork_core::ports::a2a_task_repo::A2aTaskRepository> =
        Arc::new(ork_persistence::postgres::a2a_task_repo::PgA2aTaskRepository::new(pool.clone()));
    // ADR-0008 (push notification config pulled forward from ADR-0009): the
    // dispatcher needs a real persistence layer to honour `tasks/pushNotificationConfig/*`
    // end-to-end; the webhook delivery worker stays on ADR-0009.
    let a2a_push_repo: Arc<dyn ork_core::ports::a2a_push_repo::A2aPushConfigRepository> = Arc::new(
        ork_persistence::postgres::a2a_push_repo::PgA2aPushConfigRepository::new(pool.clone()),
    );

    // ADR-0008 §`SSE bridge`: middle tier of the three-tier replay strategy.
    // Falls back to the in-memory variant when Redis is unreachable so a single
    // dev box can still serve `message/stream` (with the obvious caveat that
    // reconnecting to a different node loses the buffer).
    let sse_buffer: Arc<dyn ork_api::sse_buffer::SseBuffer> = match redis::Client::open(
        config.redis.url.as_str(),
    ) {
        Ok(client) => match redis::aio::ConnectionManager::new(client).await {
            Ok(conn) => Arc::new(ork_api::sse_buffer::RedisSseBuffer::new(
                conn,
                config.kafka.namespace.clone(),
                Duration::from_secs(60),
            )),
            Err(e) => {
                tracing::warn!(error = %e, "ADR-0008: Redis unreachable, using in-memory SSE buffer");
                Arc::new(ork_api::sse_buffer::InMemorySseBuffer::new(
                    Duration::from_secs(60),
                ))
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "ADR-0008: invalid Redis URL, using in-memory SSE buffer");
            Arc::new(ork_api::sse_buffer::InMemorySseBuffer::new(
                Duration::from_secs(60),
            ))
        }
    };

    let tenant_service = Arc::new(ork_core::services::tenant::TenantService::new(tenant_repo));
    let workflow_service = Arc::new(ork_core::services::workflow::WorkflowService::new(
        workflow_repo.clone(),
    ));

    // ADR 0012: the global LLM is the router over the operator-side
    // catalog (`[llm.providers]` in config). Tenant overrides are
    // resolved per-call through `ServiceTenantLlmCatalog`, which wraps
    // `TenantService` so `ork-llm` does not import `ork-persistence`
    // (AGENTS.md §3.4 hexagonal). Boot fails loud if any `env`-form
    // header references a variable that isn't set — operators learn
    // about a typo before the first request, not after.
    let llm_catalog: Arc<dyn ork_llm::router::TenantLlmCatalog> = Arc::new(
        ork_api::llm_catalog::ServiceTenantLlmCatalog::new(tenant_service.clone()),
    );
    let llm_provider: Arc<dyn ork_core::ports::llm::LlmProvider> = Arc::new(
        ork_llm::router::LlmRouter::from_config(&config.llm, llm_catalog)
            .context("ADR 0012: failed to build LlmRouter from [llm] config")?,
    );

    let mut integration_executor = ork_integrations::tools::IntegrationToolExecutor::new();

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let base_url = std::env::var("GITHUB_BASE_URL").ok();
        if let Ok(adapter) =
            ork_integrations::github::GitHubAdapter::new(&token, base_url.as_deref())
        {
            integration_executor.register_adapter("github", Arc::new(adapter));
            info!("GitHub integration enabled");
        }
    }

    if let Ok(token) = std::env::var("GITLAB_TOKEN") {
        let base_url = std::env::var("GITLAB_BASE_URL").ok();
        let adapter = ork_integrations::gitlab::GitLabAdapter::new(&token, base_url.as_deref());
        integration_executor.register_adapter("gitlab", Arc::new(adapter));
        info!("GitLab integration enabled");
    }

    let specs: Vec<ork_core::ports::workspace::RepositorySpec> = config
        .repositories
        .iter()
        .map(|r| ork_core::ports::workspace::RepositorySpec {
            name: r.name.clone(),
            url: r.url.clone(),
            default_branch: r.default_branch.clone(),
        })
        .collect();

    let cache = ork_integrations::workspace::expand_cache_dir(&config.workspace.cache_dir);
    let code_executor = if specs.is_empty() {
        None
    } else {
        Some(Arc::new(
            ork_integrations::code_tools::CodeToolExecutor::new(Arc::new(
                ork_integrations::workspace::GitRepoWorkspace::new(
                    cache,
                    config.workspace.clone_depth,
                    specs,
                ),
            )),
        ))
    };

    // Build the eventing client up-front so the ADR-0006 fire-and-forget delegation
    // publisher can share the same producer the discovery loop uses below.
    let eventing = ork_eventing::build_client(&config.kafka)
        .await
        .context("failed to initialise eventing client (ADR-0004)")?;

    let delegation_publisher: Arc<dyn ork_core::ports::delegation_publisher::DelegationPublisher> =
        Arc::new(ork_eventing::KafkaDelegationPublisher::new(
            eventing.producer.clone(),
            config.kafka.namespace.clone(),
        ));

    // ADR-0010: boot the MCP client before the cyclic registry init so we
    // can both (a) attach it to `CompositeToolExecutor` and (b) drive its
    // refresh loop from the same `discovery_cancel` token as every other
    // background task. `[mcp] enabled = false` yields `None`, in which
    // case we skip `with_mcp` entirely and `mcp:*` calls report a clear
    // "MCP is not configured" error from `CompositeToolExecutor`.
    //
    // ork-mcp builds its own internal `reqwest::Client` (pinned to 0.13
    // to satisfy `rmcp 0.16`'s `StreamableHttpClient` trait) so we hand
    // it just the user-agent string here; see `ork_mcp::build_from_config`
    // doc-comment for the version-skew rationale.
    let mcp_client = ork_mcp::build_from_config(&config.mcp, &config.a2a_client.user_agent)
        .context("failed to initialise MCP client (ADR 0010)")?;
    if let Some(client) = &mcp_client {
        info!(
            servers = client.sources().global_len(),
            "ADR-0010: MCP client initialised"
        );
    } else {
        info!("ADR-0010: MCP client disabled by config");
    }

    // ADR-0016: blob store, Postgres index, and tool executor; `[artifacts] enabled = false` skips all three.
    let artifact_public_base = ork_api::artifact_inbound::artifact_public_base_url(&config);
    let (artifact_store, artifact_meta, artifact_tool_exec) = if config.artifacts.enabled {
        let store = ork_api::artifacts_boot::build_artifact_store(&config)
            .await
            .context("ADR-0016: build ArtifactStore")?;
        let meta: Arc<dyn ork_core::ports::artifact_meta_repo::ArtifactMetaRepo> = Arc::new(
            ork_persistence::postgres::artifact_meta_repo::PgArtifactMetaRepo::new(pool.clone()),
        );
        let exec = Arc::new(ork_integrations::artifact_tools::ArtifactToolExecutor::new(
            store.clone(),
            meta.clone(),
            Some(artifact_public_base.clone()),
        ));
        (Some(store), Some(meta), Some(exec))
    } else {
        (None, None, None)
    };

    // ADR-0007: shared HTTP client + Redis-backed card cache + remote-agent builder.
    // Built after ADR-0016 artifact wiring so the builder can add outbound `Part::File` rewrites.
    let http_client = reqwest::Client::builder()
        .user_agent(config.a2a_client.user_agent.clone())
        .build()
        .context("failed to build shared reqwest client (ADR-0007)")?;
    let card_cache = remote_agents::build_card_cache(&config.redis.url).await;
    let a2a_artifacts = match (&artifact_store, &artifact_meta) {
        (Some(s), Some(m)) => Some((s.clone(), m.clone(), artifact_public_base.clone())),
        _ => None,
    };
    let a2a_builder = remote_agents::build_remote_builder(
        http_client.clone(),
        card_cache.clone(),
        &config.a2a_client,
        Some(delegation_publisher.clone()),
        a2a_artifacts,
    );
    let remote_builder: Arc<dyn RemoteAgentBuilder> = a2a_builder.clone();

    let card_ctx = CardEnrichmentContext {
        public_base_url: config.discovery.public_base_url.clone(),
        provider_organization: config.discovery.provider_organization.clone(),
        devportal_url: config.discovery.devportal_url.clone(),
        namespace: config.kafka.namespace.clone(),
        include_tenant_required_ext: config.discovery.include_tenant_required_ext,
        tenant_header: "X-Tenant-Id".to_string(),
    };

    // ADR 0006 wiring is cyclic: `LocalAgent`s own the composite `ToolExecutor`,
    // which owns the `AgentCallToolExecutor`, which needs to resolve targets through
    // the `AgentRegistry` that owns those same `LocalAgent`s. We resolve it with
    // `Arc::new_cyclic`: the `agent_call` executor holds a `Weak<AgentRegistry>`
    // that upgrades inside `execute` to the very `Arc` returned here.
    let agent_registry = Arc::new_cyclic(|registry_weak| {
        let agent_call_exec = Arc::new(ork_integrations::agent_call::AgentCallToolExecutor::new(
            registry_weak.clone(),
            Some(delegation_publisher.clone()),
            Some(a2a_task_repo.clone()),
        ));
        let mut composite = ork_integrations::tools::CompositeToolExecutor::new(
            integration_executor,
            code_executor,
        )
        .with_agent_call(agent_call_exec)
        .with_artifacts(artifact_tool_exec.clone());
        // ADR-0010: route `mcp:<server>.<tool>` calls through the MCP
        // client. The trait-object cast lines up with
        // `CompositeToolExecutor::with_mcp(Arc<dyn ToolExecutor>)`.
        if let Some(mcp) = mcp_client.clone() {
            let mcp_exec: Arc<dyn ork_core::workflow::engine::ToolExecutor> = mcp;
            composite = composite.with_mcp(mcp_exec);
        }
        let tool_executor: Arc<dyn ork_core::workflow::engine::ToolExecutor> = Arc::new(composite);
        let mut catalog = ork_agents::tool_catalog::ToolCatalogBuilder::new()
            .with_registry(registry_weak.clone());
        if let Some(mcp) = mcp_client.clone() {
            let mcp_catalog: Arc<dyn ork_agents::tool_catalog::McpToolCatalog> = mcp;
            catalog = catalog.with_mcp(mcp_catalog);
        }
        ork_agents::registry::build_default_registry_with_catalog(
            &card_ctx,
            llm_provider,
            tool_executor,
            catalog,
        )
    });

    // ADR-0006: a per-process cancellation token. ADR-0008 will replace this with a
    // per-run token wired through the workflow service so external `cancel` requests
    // can be honored; for v1 we share one token whose only consumer is process-shutdown.
    let run_cancel = CancellationToken::new();
    // ADR-0015: one registry + limits for workflow early resolution and API late streaming.
    let embed_registry = Arc::new(ork_core::embeds::EmbedRegistry::with_builtins());
    let embed_limits = ork_core::embeds::EmbedLimits::default();
    // ADR-0007: the same `A2aRemoteAgentBuilder` that backs `[[remote_agents]]` and the
    // discovery subscriber is also wired into the workflow engine so
    // `WorkflowAgentRef::Inline` steps build transient agents through the same HTTP
    // client, Redis card cache, retry policy, and Kafka short-circuit publisher.
    let engine = Arc::new(
        ork_core::workflow::engine::WorkflowEngine::new(workflow_repo, agent_registry.clone())
            .with_delegation(
                Some(delegation_publisher.clone()),
                Some(a2a_task_repo.clone()),
                run_cancel.clone(),
            )
            .with_remote_builder(a2a_builder.clone())
            // ADR-0009 ↔ ADR-0006: register `delegate_to.push_url` callbacks
            // so the push delivery worker fans out the child terminal-state
            // notification to the parent's chosen URL.
            .with_push_repo(a2a_push_repo.clone())
            .with_embeds(embed_registry.clone(), embed_limits.clone())
            .with_artifact_store(artifact_store.clone())
            .with_artifact_public_base(
                artifact_store
                    .as_ref()
                    .map(|_| artifact_public_base.clone()),
            ),
    );

    remote_agents::load_static_remote_agents(
        &config,
        &a2a_builder,
        &http_client,
        card_cache.clone(),
        &agent_registry,
    )
    .await;

    let discovery_cancel = CancellationToken::new();
    remote_agents::spawn_card_refresh(
        &config,
        a2a_builder.clone(),
        http_client.clone(),
        card_cache.clone(),
        agent_registry.clone(),
        discovery_cancel.clone(),
    );
    let discovery_interval = Duration::from_secs(config.discovery.interval_secs.max(1));
    let ttl_multiplier = config.discovery.ttl_multiplier.max(1);
    let namespace = config.kafka.namespace.clone();
    let local_ids = agent_registry.local_ids();
    let local_id_set = agent_registry.local_id_set();

    info!(
        interval_secs = config.discovery.interval_secs,
        ttl_multiplier,
        agents = local_ids.len(),
        "ADR-0005: starting discovery (publisher per agent + one subscriber)"
    );

    for id in &local_ids {
        let Some(agent) = agent_registry.resolve(id).await else {
            continue;
        };
        let publisher = DiscoveryPublisher::new(
            eventing.producer.clone(),
            namespace.clone(),
            id.clone(),
            discovery_interval,
            // Re-read on every tick so ADR-0014 plugins can mutate the card.
            Arc::new(move || agent.card().clone()) as CardProvider,
        );
        let cancel = discovery_cancel.clone();
        tokio::spawn(async move { publisher.run(cancel).await });
    }

    {
        let subscriber = DiscoverySubscriber::new(
            eventing.consumer.clone(),
            namespace.clone(),
            agent_registry.clone(),
            local_id_set,
            discovery_interval,
            ttl_multiplier,
        )
        .with_remote_builder(remote_builder.clone());
        let cancel = discovery_cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = subscriber.run(cancel).await {
                tracing::warn!(error = %e, "discovery subscriber exited with error");
            }
        });
    }

    {
        // TTL sweep: drops remote entries past `ttl_multiplier * discovery_interval`.
        let registry = agent_registry.clone();
        let cancel = discovery_cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(discovery_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        let dropped = registry.expire_stale(Instant::now()).await;
                        if !dropped.is_empty() {
                            tracing::info!(dropped = ?dropped, "discovery: TTL sweep evicted stale entries");
                        }
                    }
                }
            }
        });
    }

    // ADR-0009 push notifications: signing keys + JWKS provider + outbox.
    // The KEK is derived from `auth.jwt_secret` so deployments can rotate the
    // KEK by rotating the JWT signing secret (with the documented downtime).
    let signing_key_repo: Arc<dyn ork_core::ports::a2a_signing_key_repo::A2aSigningKeyRepository> =
        Arc::new(
            ork_persistence::postgres::a2a_signing_key_repo::PgA2aSigningKeyRepository::new(
                pool.clone(),
            ),
        );
    let dead_letter_repo: Arc<
        dyn ork_core::ports::a2a_push_dead_letter_repo::A2aPushDeadLetterRepository,
    > = Arc::new(
        ork_persistence::postgres::a2a_push_dead_letter_repo::PgA2aPushDeadLetterRepository::new(
            pool.clone(),
        ),
    );
    let kek = ork_push::encryption::derive_kek(&config.auth.jwt_secret);
    let rotation_policy = ork_push::signing::RotationPolicy {
        rotation_days: config.push.key_rotation_days,
        overlap_days: config.push.key_overlap_days,
    };
    let jwks_provider = ork_push::JwksProvider::new(signing_key_repo.clone(), kek, rotation_policy)
        .await
        .context("build JWKS provider")?;
    jwks_provider
        .ensure_at_least_one(chrono::Utc::now())
        .await
        .context("ensure at least one signing key on boot")?;
    let push_service = ork_push::PushService::new(eventing.clone(), namespace.clone());

    // ADR-0009 §`Delivery worker`: consume the push outbox topic, sign, and
    // POST. Reuses the boot-time `reqwest::Client` and runs under the same
    // cancellation token as the rest of the background tasks.
    {
        let worker = ork_push::worker::PushDeliveryWorker::new(
            eventing.clone(),
            namespace.clone(),
            a2a_push_repo.clone(),
            dead_letter_repo.clone(),
            jwks_provider.clone(),
            ork_push::worker::WorkerConfig {
                retry_intervals: ork_push::worker::WorkerConfig::from_minutes(
                    &config.push.retry_schedule_minutes,
                ),
                request_timeout_secs: config.push.request_timeout_secs,
                max_concurrency: config.push.max_concurrency,
                user_agent: format!("ork-push/{}", env!("CARGO_PKG_VERSION")),
            },
        );
        let cancel = discovery_cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = worker.run(cancel).await {
                tracing::warn!(error = %e, "push delivery worker exited with error");
            }
        });
    }

    // ADR-0009 §`Key rotation`: trigger `rotate_if_due` once a day so the
    // 30-day rotation cadence + 7-day overlap window happen automatically.
    {
        let provider = jwks_provider.clone();
        let cancel = discovery_cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60 * 60 * 6));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        match provider.rotate_if_due(chrono::Utc::now(), false).await {
                            Ok(Some(outcome)) => tracing::info!(
                                new_kid = %outcome.new_kid,
                                previous_kid = ?outcome.previous_kid,
                                "ADR-0009: rotated signing key"
                            ),
                            Ok(None) => {}
                            Err(e) => tracing::warn!(error = %e, "ADR-0009: rotate_if_due failed"),
                        }
                    }
                }
            }
        });
    }

    // ADR-0009 §`Janitor`: garbage-collect expired push configs.
    {
        let push_repo = a2a_push_repo.clone();
        let cancel = discovery_cancel.clone();
        let janitor_period = Duration::from_secs(60 * 60);
        let retention = chrono::Duration::days(i64::from(config.push.config_retention_days));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(janitor_period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        let cutoff = chrono::Utc::now() - retention;
                        match push_repo.delete_terminal_after(cutoff).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(count = n, "ADR-0009: janitor swept push configs"),
                            Err(e) => tracing::warn!(error = %e, "ADR-0009: janitor sweep failed"),
                        }
                    }
                }
            }
        });
    }

    // ADR-0016 §`Retention`: Postgres `eligible_for_sweep` + blob `delete` + `delete_version`.
    if let (Some(store), Some(meta)) = (artifact_store.clone(), artifact_meta.clone()) {
        ork_api::artifact_retention::spawn_artifact_retention_sweep(
            store,
            meta,
            config.retention.default_days,
            config.retention.task_artifacts_days,
            config.retention.sweep_interval_secs,
            discovery_cancel.clone(),
        );
    }

    // ADR-0010 §`Tool discovery`: periodic refresh of the MCP descriptor
    // cache so newly-added server tools surface to the LLM without a
    // restart. The interval is bounded below by 1s so a `0` in config
    // can't busy-loop the runtime; per-server failures are logged and
    // swallowed inside `refresh_all` to keep one bad vendor from
    // poisoning the rest of the cache.
    let webui_store: std::sync::Arc<dyn ork_core::ports::webui_store::WebuiStore> =
        std::sync::Arc::new(ork_persistence::postgres::webui_store::PgWebuiStore::new(
            pool.clone(),
        ));

    if let Some(client) = mcp_client.clone() {
        let cancel = discovery_cancel.clone();
        let interval = config.mcp.refresh_interval();
        info!(
            interval_secs = interval.as_secs(),
            "ADR-0010: starting MCP descriptor refresh loop"
        );
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        if let Err(e) = client.refresh_all().await {
                            tracing::warn!(error = %e, "ADR-0010: mcp refresh_all failed");
                        }
                    }
                }
            }
        });
    }

    let app_state = state::AppState {
        config: config.clone(),
        tenant_service,
        workflow_service,
        agent_registry,
        engine,
        eventing,
        remote_builder,
        a2a_task_repo,
        a2a_push_repo,
        sse_buffer,
        push_service,
        jwks_provider,
        embed_registry,
        embed_limits,
        artifact_store: artifact_store.clone(),
        artifact_meta: artifact_meta.clone(),
        artifact_public_base: artifact_public_base.clone(),
        webui_store,
    };

    let gateway_boot =
        ork_api::gateways::build_and_start_gateways(&app_state, discovery_cancel.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("configure generic gateways (ADR-0013)")?;

    let app = routes::create_router_with_gateways(
        app_state,
        gateway_boot.router,
        gateway_boot.protected_router,
    );

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr = %addr, "ork server starting");
    axum::serve(listener, app).await?;

    // Cancel shared discovery/background work first (agent + gateway card publishers, event-mesh
    // consumer `child_token` chains, etc.).
    info!(
        "server stopped; cancelling discovery and gateway background tasks (publishers will flush `died` tombstones)"
    );
    discovery_cancel.cancel();
    // Give publishers ~one publish cycle to emit their tombstones before exit.
    tokio::time::sleep(discovery_interval.min(Duration::from_secs(2))).await;

    for g in gateway_boot.gateways {
        if let Err(e) = g.shutdown().await {
            tracing::warn!(error = %e, gateway_id = %g.id(), "gateway shutdown");
        }
    }

    Ok(())
}
