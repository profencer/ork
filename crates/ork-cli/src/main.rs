use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use futures::StreamExt;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::models::workflow::{
    WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::ports::integration::{RepoQuery, SourceControlAdapter};
use ork_core::ports::llm::{ChatMessage, ChatRequest, ChatStreamEvent, LlmProvider};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::{ToolExecutor, WorkflowEngine};

#[derive(Parser)]
#[command(name = "ork", about = "Business flow automation CLI")]
struct Cli {
    /// Enable verbose output (show config, full error details)
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a standup brief from your recent commits, PRs, and issues
    Standup {
        /// Repositories to scan (format: owner/repo). Can be repeated.
        #[arg(required = true)]
        repos: Vec<String>,

        /// How many hours back to look (default: 24)
        #[arg(short = 'H', long, default_value = "24")]
        hours: u64,

        /// Filter commits to a specific author name or email (substring match)
        #[arg(short, long)]
        author: Option<String>,

        /// Use GitHub as the source (default if GITHUB_TOKEN is set)
        #[arg(long)]
        github: bool,

        /// Use GitLab as the source (default if GITLAB_TOKEN is set and GITHUB_TOKEN is not)
        #[arg(long)]
        gitlab: bool,

        /// GitHub Enterprise API base URL (e.g. https://github.example.com/api/v3)
        #[arg(long)]
        github_url: Option<String>,

        /// GitLab base URL (default: https://gitlab.com)
        #[arg(long)]
        gitlab_url: Option<String>,

        /// Skip AI summarization and just print raw activity
        #[arg(long)]
        raw: bool,
    },
    /// Run the change-plan workflow locally (clones repos, code search, multi-agent plan)
    ChangePlan {
        /// Natural-language task (e.g. "Add tenant-scoped rate limiting")
        task: String,
        /// Workflow YAML (default: workflow-templates/change-plan.yaml)
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Hide full LLM replies on stderr (tracing progress still prints)
        #[arg(long)]
        no_print_llm: bool,
    },
    /// Administrative operations (DB-bound).
    Admin {
        #[command(subcommand)]
        cmd: AdminCommand,
    },
    /// Workflow file utilities.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Push notification administration (ADR-0009).
    Push {
        #[command(subcommand)]
        cmd: PushAdminCommand,
    },
}

#[derive(Subcommand)]
enum PushAdminCommand {
    /// Force generation of a new ES256 signing key. The previous key stays
    /// in JWKS for the configured overlap window so subscribers cached by
    /// `kid` keep verifying in-flight requests.
    RotateKeys,
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Migrate legacy step tools into prompt hints for ADR-0011.
    MigrateTools {
        /// Workflow YAML file, or directory to scan recursively for *.yaml/*.yml.
        path: PathBuf,
        /// Rewrite files in place instead of printing a diff.
        #[arg(long)]
        in_place: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let verbose = cli.verbose;

    match cli.command {
        Commands::Standup {
            repos,
            hours,
            author,
            github,
            gitlab,
            github_url,
            gitlab_url,
            raw,
        } => {
            run_standup(
                repos, hours, author, github, gitlab, github_url, gitlab_url, raw, verbose,
            )
            .await?;
        }
        Commands::ChangePlan {
            task,
            file,
            no_print_llm,
        } => {
            run_change_plan(task, file, verbose, no_print_llm).await?;
        }
        Commands::Admin { cmd } => match cmd {
            AdminCommand::Push { cmd } => match cmd {
                PushAdminCommand::RotateKeys => {
                    run_admin_push_rotate_keys(verbose).await?;
                }
            },
        },
        Commands::Workflow { cmd } => match cmd {
            WorkflowCmd::MigrateTools { path, in_place } => {
                run_workflow_migrate_tools(path, in_place)?;
            }
        },
    }

    Ok(())
}

/// `ork admin push rotate-keys` — opens the same Postgres pool the API uses,
/// derives the KEK from `auth.jwt_secret`, and triggers a forced rotation.
/// Prints the new `kid`, expiry, and the rotated-out predecessor (if any) on
/// stdout in a small JSON envelope so operators can pipe it through `jq`.
async fn run_admin_push_rotate_keys(verbose: bool) -> Result<()> {
    let config = ork_common::config::AppConfig::load()
        .context("load AppConfig (ORK__ env or config/default.toml)")?;
    if verbose {
        eprintln!(
            "Connecting to Postgres at {} (max_connections={})",
            config.database.url, config.database.max_connections
        );
    }
    let pool = ork_persistence::postgres::create_pool(
        &config.database.url,
        config.database.max_connections,
    )
    .await
    .context("connect to database")?;
    let repo: Arc<dyn ork_core::ports::a2a_signing_key_repo::A2aSigningKeyRepository> = Arc::new(
        ork_persistence::postgres::a2a_signing_key_repo::PgA2aSigningKeyRepository::new(pool),
    );
    let kek = ork_push::encryption::derive_kek(&config.auth.jwt_secret);
    let policy = ork_push::signing::RotationPolicy {
        rotation_days: config.push.key_rotation_days,
        overlap_days: config.push.key_overlap_days,
    };
    let provider = ork_push::JwksProvider::new(repo, kek, policy)
        .await
        .context("build JWKS provider")?;
    let outcome = provider
        .rotate_if_due(Utc::now(), true)
        .await
        .context("rotate signing key")?;
    match outcome {
        Some(o) => {
            let body = serde_json::json!({
                "rotated": true,
                "new_kid": o.new_kid,
                "new_expires_at": o.new_expires_at,
                "previous_kid": o.previous_kid,
            });
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        None => {
            // `force=true` should always rotate; if not, surface clearly.
            println!("{{\"rotated\": false}}");
        }
    }
    Ok(())
}

/// YAML workflow file shape (no DB ids); matches `workflow-templates/*.yaml`.
#[derive(Debug, Deserialize, Serialize)]
struct WorkflowYaml {
    name: String,
    version: String,
    trigger: WorkflowTrigger,
    steps: Vec<WorkflowStep>,
}

const TOOL_MIGRATION_MARKER: &str = "Use the following tools as needed:";

fn migrate_step(step: &mut WorkflowStep) -> bool {
    if step.tools.is_empty() || step.prompt_template.contains(TOOL_MIGRATION_MARKER) {
        return false;
    }
    let hint = format!("{TOOL_MIGRATION_MARKER} {}.\n\n", step.tools.join(", "));
    step.prompt_template = format!("{hint}{}", step.prompt_template);
    true
}

fn collect_yaml_files(path: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        bail!("{} is neither a file nor a directory", path.display());
    }
    for entry in std::fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_yaml_files(&child, out)?;
        } else if matches!(
            child.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        ) {
            out.push(child);
        }
    }
    Ok(())
}

fn simple_unified_diff(path: &std::path::Path, before: &str, after: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- {}\n", path.display()));
    out.push_str(&format!("+++ {}\n", path.display()));
    out.push_str("@@\n");
    for line in before.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in after.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn run_workflow_migrate_tools(path: PathBuf, in_place: bool) -> Result<()> {
    let mut files = Vec::new();
    collect_yaml_files(&path, &mut files)?;
    files.sort();

    let mut migrated_steps = 0usize;
    let mut changed_files = 0usize;
    for file in files {
        let before = std::fs::read_to_string(&file)
            .with_context(|| format!("read workflow file {}", file.display()))?;
        let mut wf: WorkflowYaml = serde_yaml::from_str(&before)
            .with_context(|| format!("parse YAML {}", file.display()))?;
        let mut changed_steps = 0usize;
        for step in &mut wf.steps {
            if migrate_step(step) {
                changed_steps += 1;
            }
        }
        if changed_steps == 0 {
            continue;
        }
        migrated_steps += changed_steps;
        changed_files += 1;
        let after = serde_yaml::to_string(&wf)
            .with_context(|| format!("serialize YAML {}", file.display()))?;
        if in_place {
            std::fs::write(&file, after)
                .with_context(|| format!("write workflow file {}", file.display()))?;
        } else {
            print!("{}", simple_unified_diff(&file, &before, &after));
        }
    }
    eprintln!("migrated {migrated_steps} step(s) across {changed_files} file(s)");
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

async fn run_change_plan(
    task: String,
    file: Option<PathBuf>,
    verbose: bool,
    no_print_llm: bool,
) -> Result<()> {
    // SAFETY: `set_var` is unsafe in Rust 2024; we run once at CLI startup before other threads.
    unsafe {
        if no_print_llm {
            std::env::set_var("ORK_PRINT_LLM_OUTPUT", "0");
        } else if std::env::var("ORK_PRINT_LLM_OUTPUT").is_err() {
            std::env::set_var("ORK_PRINT_LLM_OUTPUT", "1");
        }
    }

    let key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
    if key.is_empty() {
        bail!("MINIMAX_API_KEY is required for change-plan (LLM steps).");
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

    let llm: Arc<dyn LlmProvider> = Arc::new(ork_llm::minimax::MinimaxProvider::new(
        key,
        Some(config.llm.base_url.clone()),
        Some(config.llm.model.clone()),
    ));

    let tool_executor = build_cli_tool_executor(&config)?;
    let card_ctx = ork_core::a2a::card_builder::CardEnrichmentContext {
        public_base_url: config.discovery.public_base_url.clone(),
        provider_organization: config.discovery.provider_organization.clone(),
        devportal_url: config.discovery.devportal_url.clone(),
        namespace: config.kafka.namespace.clone(),
        include_tenant_required_ext: config.discovery.include_tenant_required_ext,
        tenant_header: "X-Tenant-Id".to_string(),
    };
    let agent_registry = Arc::new(ork_agents::registry::build_default_registry(
        &card_ctx,
        llm,
        tool_executor,
    ));
    let engine = Arc::new(WorkflowEngine::new(
        Arc::new(NoopWorkflowRepository::default()),
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

    if verbose {
        if let Some(review) = run
            .step_results
            .iter()
            .find(|s| s.step_id == "review")
            .and_then(|s| s.output.as_deref())
        {
            eprintln!("\n── reviewer ────────────────────────────────\n{review}");
        }
    }

    Ok(())
}

fn build_cli_tool_executor(
    config: &ork_common::config::AppConfig,
) -> Result<Arc<dyn ToolExecutor>> {
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

    Ok(Arc::new(
        ork_integrations::tools::CompositeToolExecutor::new(integration_executor, code_executor),
    ))
}

fn mask_token(token: &str) -> String {
    if token.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &token[..4], &token[token.len() - 4..])
}

async fn run_standup(
    repos: Vec<String>,
    hours: u64,
    author_filter: Option<String>,
    force_github: bool,
    force_gitlab: bool,
    github_url: Option<String>,
    gitlab_url: Option<String>,
    raw: bool,
    verbose: bool,
) -> Result<()> {
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let gitlab_token = std::env::var("GITLAB_TOKEN").ok();
    let github_url = github_url.or_else(|| std::env::var("GITHUB_BASE_URL").ok());
    let gitlab_url = gitlab_url.or_else(|| std::env::var("GITLAB_BASE_URL").ok());
    let minimax_key = std::env::var("MINIMAX_API_KEY").ok();

    let use_github = force_github || (!force_gitlab && github_token.is_some());
    let use_gitlab = force_gitlab || (!force_github && gitlab_token.is_some() && !use_github);

    if verbose {
        eprintln!("── Config ──────────────────────────────────");
        eprintln!(
            "  GITHUB_TOKEN:    {}",
            match &github_token {
                Some(t) => mask_token(t),
                None => "(not set)".into(),
            }
        );
        eprintln!(
            "  GITHUB_BASE_URL: {}",
            github_url
                .as_deref()
                .unwrap_or("(not set, using api.github.com)")
        );
        eprintln!(
            "  GITLAB_TOKEN:    {}",
            match &gitlab_token {
                Some(t) => mask_token(t),
                None => "(not set)".into(),
            }
        );
        eprintln!(
            "  GITLAB_BASE_URL: {}",
            gitlab_url
                .as_deref()
                .unwrap_or("(not set, using gitlab.com)")
        );
        eprintln!(
            "  MINIMAX_API_KEY: {}",
            match &minimax_key {
                Some(t) => mask_token(t),
                None => "(not set)".into(),
            }
        );
        eprintln!(
            "  Provider:        {}",
            if use_github {
                "github"
            } else if use_gitlab {
                "gitlab"
            } else {
                "none"
            }
        );
        eprintln!("  Repos:           {}", repos.join(", "));
        eprintln!("  Hours:           {hours}");
        if let Some(ref a) = author_filter {
            eprintln!("  Author filter:   {a}");
        }
        eprintln!("────────────────────────────────────────────\n");
    }

    if !use_github && !use_gitlab {
        bail!(
            "No source control token found.\n\
             Set GITHUB_TOKEN or GITLAB_TOKEN environment variable, \
             or use --github / --gitlab flags."
        );
    }

    let adapter: Arc<dyn SourceControlAdapter> = if use_github {
        let token = github_token.context("GITHUB_TOKEN not set")?;
        if verbose {
            eprintln!(
                "Connecting to GitHub{}...",
                github_url
                    .as_ref()
                    .map(|u| format!(" at {u}"))
                    .unwrap_or_default()
            );
        }
        Arc::new(
            ork_integrations::github::GitHubAdapter::new(&token, github_url.as_deref())
                .context("Failed to create GitHub client")?,
        )
    } else {
        let token = gitlab_token.context("GITLAB_TOKEN not set")?;
        if verbose {
            eprintln!(
                "Connecting to GitLab{}...",
                gitlab_url
                    .as_ref()
                    .map(|u| format!(" at {u}"))
                    .unwrap_or_default()
            );
        }
        Arc::new(ork_integrations::gitlab::GitLabAdapter::new(
            &token,
            gitlab_url.as_deref(),
        ))
    };

    let since = Utc::now() - Duration::hours(hours as i64);
    let provider = adapter.provider_name();

    eprintln!(
        "Fetching activity from {} repos on {} (last {} hours)...\n",
        repos.len(),
        provider,
        hours
    );

    let mut all_commits = Vec::new();
    let mut all_prs = Vec::new();
    let mut all_issues = Vec::new();

    for repo_str in &repos {
        let (owner, repo) = parse_repo(repo_str)?;
        if verbose {
            eprintln!("  → {owner}/{repo}");
        }
        let query = RepoQuery {
            owner: owner.clone(),
            repo: repo.clone(),
            since: Some(since),
            until: None,
            branch: None,
        };

        match adapter.list_recent_commits(&query).await {
            Ok(commits) => {
                if verbose {
                    eprintln!("    commits: {} found", commits.len());
                }
                let filtered = if let Some(ref filter) = author_filter {
                    let f = filter.to_lowercase();
                    commits
                        .into_iter()
                        .filter(|c| c.author.to_lowercase().contains(&f))
                        .collect()
                } else {
                    commits
                };
                for mut c in filtered {
                    c.url = format!("[{}/{}] {}", owner, repo, c.url);
                    all_commits.push((format!("{owner}/{repo}"), c));
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to fetch commits from {repo_str}: {e}");
                if verbose {
                    eprintln!("  Debug:   {e:?}");
                }
            }
        }

        match adapter.list_pull_requests(&query, None).await {
            Ok(prs) => {
                if verbose {
                    eprintln!("    PRs:     {} found", prs.len());
                }
                let filtered = if let Some(ref filter) = author_filter {
                    let f = filter.to_lowercase();
                    prs.into_iter()
                        .filter(|p| p.author.to_lowercase().contains(&f))
                        .collect()
                } else {
                    prs
                };
                for pr in filtered {
                    all_prs.push((format!("{owner}/{repo}"), pr));
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to fetch PRs from {repo_str}: {e}");
                if verbose {
                    eprintln!("  Debug:   {e:?}");
                }
            }
        }

        match adapter.list_issues(&query, Some("open")).await {
            Ok(issues) => {
                if verbose {
                    eprintln!("    issues:  {} found", issues.len());
                }
                for issue in issues {
                    all_issues.push((format!("{owner}/{repo}"), issue));
                }
            }
            Err(e) => {
                eprintln!("  Warning: failed to fetch issues from {repo_str}: {e}");
                if verbose {
                    eprintln!("  Debug:   {e:?}");
                }
            }
        }
    }

    if verbose {
        eprintln!();
    }

    if all_commits.is_empty() && all_prs.is_empty() && all_issues.is_empty() {
        println!("No activity found in the last {} hours.", hours);
        return Ok(());
    }

    let activity_text = format_activity(&all_commits, &all_prs, &all_issues);

    if raw {
        println!("{activity_text}");
        return Ok(());
    }

    if let Some(api_key) = minimax_key {
        eprintln!("Generating AI standup summary...\n");

        let llm = ork_llm::minimax::MinimaxProvider::new(api_key, None, None);
        let request = ChatRequest {
            messages: vec![
                ChatMessage::system(STANDUP_SYSTEM_PROMPT),
                ChatMessage::user(format!(
                    "Here is my development activity from the last {hours} hours. \
                         Generate a standup brief I can use in my daily standup meeting.\n\n\
                         {activity_text}"
                )),
            ],
            temperature: Some(0.3),
            max_tokens: Some(2048),
            model: None,
            tools: Vec::new(),
            tool_choice: None,
        };

        match llm.chat_stream(request).await {
            Ok(mut stream) => {
                eprintln!("── standup summary (streaming) ──");
                let mut content = String::new();
                let mut stream_err: Option<ork_common::error::OrkError> = None;
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(ChatStreamEvent::Delta(d)) => {
                            content.push_str(&d);
                            eprint!("{d}");
                            let _ = std::io::stderr().flush();
                        }
                        Ok(ChatStreamEvent::Done { .. }) => break,
                        Ok(ChatStreamEvent::ToolCall(_))
                        | Ok(ChatStreamEvent::ToolCallDelta { .. }) => {}
                        Err(e) => {
                            stream_err = Some(e);
                            break;
                        }
                    }
                }
                eprintln!();
                if let Some(e) = stream_err {
                    eprintln!("AI summarization failed: {e}");
                    if verbose {
                        eprintln!("Debug: {e:?}");
                    }
                    eprintln!("Falling back to raw output.\n");
                    println!("{activity_text}");
                } else {
                    println!("{content}");
                }
            }
            Err(e) => {
                eprintln!("AI summarization failed: {e}");
                if verbose {
                    eprintln!("Debug: {e:?}");
                }
                eprintln!("Falling back to raw output.\n");
                println!("{activity_text}");
            }
        }
    } else {
        println!("{activity_text}");
        eprintln!("\nTip: Set MINIMAX_API_KEY to get an AI-generated standup summary.");
    }

    Ok(())
}

fn parse_repo(s: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = s.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        bail!("Invalid repo format '{}'. Expected: owner/repo", s);
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn format_activity(
    commits: &[(String, ork_core::ports::integration::CommitInfo)],
    prs: &[(String, ork_core::ports::integration::PullRequestInfo)],
    issues: &[(String, ork_core::ports::integration::IssueInfo)],
) -> String {
    let mut out = String::new();

    if !commits.is_empty() {
        out.push_str(&format!("## Commits ({})\n\n", commits.len()));
        for (repo, c) in commits {
            let first_line = c.message.lines().next().unwrap_or(&c.message);
            let short_sha = &c.sha[..7.min(c.sha.len())];
            let time = c.timestamp.format("%H:%M");
            out.push_str(&format!(
                "- `{short_sha}` [{repo}] {first_line} ({time}, {})\n",
                c.author
            ));
        }
        out.push('\n');
    }

    if !prs.is_empty() {
        out.push_str(&format!("## Pull Requests ({})\n\n", prs.len()));
        for (repo, pr) in prs {
            let status = if pr.merged_at.is_some() {
                "merged"
            } else {
                &pr.state
            };
            out.push_str(&format!(
                "- #{} [{repo}] {} [{}] (by {})\n",
                pr.number, pr.title, status, pr.author
            ));
        }
        out.push('\n');
    }

    if !issues.is_empty() {
        out.push_str(&format!("## Open Issues ({})\n\n", issues.len()));
        for (repo, issue) in issues {
            let labels = if issue.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", issue.labels.join(", "))
            };
            out.push_str(&format!(
                "- #{} [{repo}] {}{}\n",
                issue.number, issue.title, labels
            ));
        }
        out.push('\n');
    }

    out
}

const STANDUP_SYSTEM_PROMPT: &str = r#"You are a concise standup brief generator for a software developer.

Given a list of recent commits, pull requests, and issues, produce a brief standup update.

Format:
1. **Yesterday / Recent Work** — 3-5 bullet points summarizing what was accomplished, derived from commits and merged PRs
2. **In Progress** — any open PRs or work that's still ongoing
3. **Blockers / Attention Needed** — any open issues or failed items worth mentioning (or "None" if clear)

Rules:
- Be concise — each bullet should be one line
- Group related commits into a single bullet point (don't list every commit separately)
- Use plain language, not commit hash jargon
- If there are many commits on the same topic, summarize them as one item
- Keep the entire output under 200 words"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn step(tools: Vec<&str>, prompt: &str) -> WorkflowStep {
        WorkflowStep {
            id: "s".into(),
            agent: "writer".into(),
            tools: tools.into_iter().map(String::from).collect(),
            prompt_template: prompt.into(),
            depends_on: Vec::new(),
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to: None,
        }
    }

    #[test]
    fn migrate_step_prepends_tool_hint() {
        let mut step = step(vec!["read_file", "code_search"], "Do work.");
        assert!(migrate_step(&mut step));
        assert_eq!(
            step.prompt_template,
            "Use the following tools as needed: read_file, code_search.\n\nDo work."
        );
    }

    #[test]
    fn migrate_step_is_idempotent() {
        let mut step = step(
            vec!["read_file"],
            "Use the following tools as needed: read_file.\n\nDo work.",
        );
        assert!(!migrate_step(&mut step));
    }

    #[test]
    fn migrate_step_skips_empty_tool_list() {
        let mut step = step(vec![], "Do work.");
        assert!(!migrate_step(&mut step));
        assert_eq!(step.prompt_template, "Do work.");
    }

    #[test]
    fn migrate_change_plan_template_diff_contains_tool_hints() {
        let before = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../workflow-templates/change-plan.yaml"
        ))
        .unwrap();
        let mut wf: WorkflowYaml = serde_yaml::from_str(&before).unwrap();
        let mut changed = 0usize;
        for step in &mut wf.steps {
            if migrate_step(step) {
                changed += 1;
            }
        }
        let after = serde_yaml::to_string(&wf).unwrap();
        let diff = simple_unified_diff(
            std::path::Path::new("workflow-templates/change-plan.yaml"),
            &before,
            &after,
        );
        assert_eq!(changed, 2);
        assert!(diff.contains("Use the following tools as needed: list_repos."));
        assert!(
            diff.contains("Use the following tools as needed: code_search, read_file, list_tree.")
        );
    }
}
