//! `ork legacy standup` — fetch recent commits/PRs/issues and (optionally)
//! summarise them via an LLM. Verbatim port of the original `ork standup`
//! verb (ADR-0057 §`Legacy subcommands`).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use clap::Args;
use futures::StreamExt;
use ork_core::ports::integration::{RepoQuery, SourceControlAdapter};
use ork_core::ports::llm::{ChatMessage, ChatRequest, ChatStreamEvent, LlmProvider};
use std::io::Write;

#[derive(Args)]
pub struct StandupArgs {
    /// Repositories to scan (format: owner/repo). Can be repeated.
    #[arg(required = true)]
    pub repos: Vec<String>,

    /// How many hours back to look (default: 24)
    #[arg(short = 'H', long, default_value = "24")]
    pub hours: u64,

    /// Filter commits to a specific author name or email (substring match)
    #[arg(short, long)]
    pub author: Option<String>,

    /// Use GitHub as the source (default if GITHUB_TOKEN is set)
    #[arg(long)]
    pub github: bool,

    /// Use GitLab as the source (default if GITLAB_TOKEN is set and GITHUB_TOKEN is not)
    #[arg(long)]
    pub gitlab: bool,

    /// GitHub Enterprise API base URL (e.g. https://github.example.com/api/v3)
    #[arg(long)]
    pub github_url: Option<String>,

    /// GitLab base URL (default: https://gitlab.com)
    #[arg(long)]
    pub gitlab_url: Option<String>,

    /// Skip AI summarization and just print raw activity
    #[arg(long)]
    pub raw: bool,
}

pub async fn run(args: StandupArgs, verbose: bool) -> Result<()> {
    let StandupArgs {
        repos,
        hours,
        author: author_filter,
        github: force_github,
        gitlab: force_gitlab,
        github_url,
        gitlab_url,
        raw,
    } = args;

    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let gitlab_token = std::env::var("GITLAB_TOKEN").ok();
    let github_url = github_url.or_else(|| std::env::var("GITHUB_BASE_URL").ok());
    let gitlab_url = gitlab_url.or_else(|| std::env::var("GITLAB_BASE_URL").ok());

    let app_config = ork_common::config::AppConfig::load().ok();
    let llm_router: Option<ork_llm::router::LlmRouter> = match app_config.as_ref() {
        Some(cfg) if !cfg.llm.providers.is_empty() => {
            match ork_llm::router::LlmRouter::from_config(
                &cfg.llm,
                Arc::new(ork_llm::router::NoopTenantLlmCatalog),
            ) {
                Ok(r) => Some(r),
                Err(e) => {
                    if verbose {
                        eprintln!("LLM router failed to build, raw output only: {e}");
                    }
                    None
                }
            }
        }
        _ => None,
    };

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
            "  LLM router:      {}",
            if llm_router.is_some() {
                "configured"
            } else {
                "(no providers; raw output only)"
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

    if let Some(llm) = &llm_router {
        eprintln!("Generating AI standup summary...\n");

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
            provider: None,
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
        eprintln!(
            "\nTip: configure an [[llm.providers]] entry in config/default.toml \
             (ADR 0012) to get an AI-generated standup summary."
        );
    }

    Ok(())
}

fn mask_token(token: &str) -> String {
    if token.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &token[..4], &token[token.len() - 4..])
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
