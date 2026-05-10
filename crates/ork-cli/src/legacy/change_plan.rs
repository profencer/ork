//! `ork legacy change-plan` — runs the change-plan workflow locally
//! (clones repos, code search, multi-agent plan). ADR-0057 §`Legacy
//! subcommands`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Args;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::models::workflow::{
    WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::ports::llm::LlmProvider;
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;
use serde::{Deserialize, Serialize};

#[derive(Args)]
pub struct ChangePlanArgs {
    /// Natural-language task (e.g. "Add tenant-scoped rate limiting")
    pub task: String,
    /// Workflow YAML (default: workflow-templates/change-plan.yaml)
    #[arg(short, long)]
    pub file: Option<PathBuf>,
    /// Hide full LLM replies on stderr (tracing progress still prints)
    #[arg(long)]
    pub no_print_llm: bool,
}

/// YAML workflow file shape (no DB ids); matches `workflow-templates/*.yaml`.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowYaml {
    pub name: String,
    pub version: String,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStep>,
}

pub async fn run(args: ChangePlanArgs, verbose: bool) -> Result<()> {
    let ChangePlanArgs {
        task,
        file,
        no_print_llm,
    } = args;

    // SAFETY: `set_var` is unsafe in Rust 2024; we run once at CLI startup before other threads.
    unsafe {
        if no_print_llm {
            std::env::set_var("ORK_PRINT_LLM_OUTPUT", "0");
        } else if std::env::var("ORK_PRINT_LLM_OUTPUT").is_err() {
            std::env::set_var("ORK_PRINT_LLM_OUTPUT", "1");
        }
    }

    let template_path =
        file.unwrap_or_else(|| PathBuf::from("workflow-templates/change-plan.yaml"));
    let yaml = std::fs::read_to_string(&template_path)
        .with_context(|| format!("read workflow file {}", template_path.display()))?;
    let wf: WorkflowYaml = serde_yaml::from_str(&yaml)
        .with_context(|| format!("parse YAML {}", template_path.display()))?;

    let config = ork_common::config::AppConfig::load().unwrap_or_else(|_| {
        eprintln!("Note: using default app config (no config/default.toml found).");
        ork_common::config::AppConfig::default()
    });

    if config.repositories.is_empty() {
        bail!(
            "No repositories configured. Add [[repositories]] blocks to config/default.toml \
             (run from the repo root so `config/default.toml` is found), or set ORK__ paths via env."
        );
    }

    if verbose {
        eprintln!("── change-plan ─────────────────────────────");
        eprintln!("  workflow:  {}", template_path.display());
        eprintln!("  task:      {task}");
        eprintln!(
            "  cache:     {} (depth {})",
            config.workspace.cache_dir, config.workspace.clone_depth
        );
        eprintln!(
            "  repos:     {}",
            config
                .repositories
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        eprintln!("────────────────────────────────────────────\n");
    }

    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: wf.name,
        version: wf.version,
        trigger: wf.trigger,
        steps: wf.steps,
        created_at: now,
        updated_at: now,
    };

    if config.llm.providers.is_empty() {
        bail!(
            "No LLM providers configured for change-plan.\n\
             Add at least one [[llm.providers]] entry to config/default.toml \
             (see ADR 0012 §`Decision`)."
        );
    }
    let llm: Arc<dyn LlmProvider> = Arc::new(
        ork_llm::router::LlmRouter::from_config(
            &config.llm,
            Arc::new(ork_llm::router::NoopTenantLlmCatalog),
        )
        .context("ADR 0012: failed to build LlmRouter from [llm] config")?,
    );

    let tool_catalog = build_cli_tool_catalog(&config)?;
    let card_ctx = ork_core::a2a::card_builder::CardEnrichmentContext {
        public_base_url: config.discovery.public_base_url.clone(),
        provider_organization: config.discovery.provider_organization.clone(),
        devportal_url: config.discovery.devportal_url.clone(),
        namespace: config.kafka.namespace.clone(),
        include_tenant_required_ext: config.discovery.include_tenant_required_ext,
        tenant_header: "X-Tenant-Id".to_string(),
    };
    let agent_registry = Arc::new(ork_agents::registry::build_default_registry_with_catalog(
        &card_ctx,
        llm,
        tool_catalog,
    ));
    let engine = Arc::new(WorkflowEngine::new(
        Arc::new(NoopWorkflowRepository),
        agent_registry,
    ));

    let graph = compiler::compile(&def).context("compile workflow graph")?;

    let mut run = WorkflowRun {
        id: WorkflowRunId::new(),
        workflow_id: def.id,
        tenant_id,
        status: WorkflowRunStatus::Pending,
        input: serde_json::json!({ "task": task }),
        output: None,
        step_results: vec![],
        started_at: Utc::now(),
        completed_at: None,
        parent_run_id: None,
        parent_step_id: None,
        parent_task_id: None,
    };

    init_change_plan_progress_logging();
    eprintln!(
        "Running workflow {:?} - progress and full LLM replies on stderr; final plan markdown on stdout.\n\
         Tip: RUST_LOG=ork_core::workflow::engine=debug for more detail; use --no-print-llm to hide LLM text.\n",
        def.name
    );
    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .context("workflow execution failed")?;

    if let Some(text) = run
        .step_results
        .iter()
        .find(|s| s.step_id == "write_plan")
        .and_then(|s| s.output.as_deref())
    {
        println!("{text}");
    } else if let Some(out) = run.output.as_ref().and_then(|v| v.as_str()) {
        println!("{out}");
    } else {
        bail!("No write_plan output and no final run output.");
    }

    if verbose
        && let Some(review) = run
            .step_results
            .iter()
            .find(|s| s.step_id == "review")
            .and_then(|s| s.output.as_deref())
    {
        eprintln!("\n── reviewer ────────────────────────────────\n{review}");
    }

    Ok(())
}

/// Prints workflow/agent progress to stderr (tracing). Honors `RUST_LOG` when set.
fn init_change_plan_progress_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "ork_core::workflow::engine=info,\
             ork_integrations::code_tools=info,\
             ork_integrations::workspace=info",
        )
    });
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .compact()
        .try_init();
}

fn build_cli_tool_catalog(
    config: &ork_common::config::AppConfig,
) -> Result<ork_agents::tool_catalog::ToolCatalogBuilder> {
    let mut integration_executor = ork_integrations::tools::IntegrationToolExecutor::new();

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let base_url = std::env::var("GITHUB_BASE_URL").ok();
        if let Ok(adapter) =
            ork_integrations::github::GitHubAdapter::new(&token, base_url.as_deref())
        {
            integration_executor.register_adapter("github", Arc::new(adapter));
        }
    }

    if let Ok(token) = std::env::var("GITLAB_TOKEN") {
        let base_url = std::env::var("GITLAB_BASE_URL").ok();
        let adapter = ork_integrations::gitlab::GitLabAdapter::new(&token, base_url.as_deref());
        integration_executor.register_adapter("gitlab", Arc::new(adapter));
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

    let mut native = HashMap::new();
    ork_integrations::native_tool_defs::extend_native_tool_map(
        &mut native,
        Arc::new(integration_executor),
        code_executor,
        None,
        None,
    );

    Ok(ork_agents::tool_catalog::ToolCatalogBuilder::new().with_native_tools(Arc::new(native)))
}
