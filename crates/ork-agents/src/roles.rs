//! Pre-baked role agents for demos and the cross-repo planning workflow.
//!
//! ADR-0052 Phase 5 reauthored writer / reviewer / synthesizer with the
//! [`CodeAgent`](crate::code_agent::CodeAgent) DSL. Planner and researcher
//! still construct via [`LocalAgent`] because they consume the tool-catalog
//! allow-list path: their tools (e.g. `list_repos`, `code_search`) are
//! discovered at request time from the ambient native-tool catalog rather
//! than supplied at build time. Once the catalog→`Vec<Arc<dyn ToolDef>>`
//! resolution moves up to `OrkApp::build()` (follow-up to ADR-0049), they
//! can move to the builder shape too.
use std::sync::Arc;

use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::LlmProvider;

use crate::CodeAgent;
use crate::local::LocalAgent;
use crate::tool_catalog::ToolCatalogBuilder;

pub fn planner_config() -> AgentConfig {
    AgentConfig {
        id: "planner".into(),
        name: "Planner".into(),
        description: "Analyzes data and produces structured plans for downstream agents.".into(),
        system_prompt: r#"You are a Planner agent for a DevOps automation platform.

Your responsibilities:
- Analyze incoming data and organize it into structured categories
- For release notes: categorize changes into Features, Bug Fixes, Breaking Changes, and Other
- For cross-repo planning: consolidate per-repository findings, ordering, shared APIs/schemas, and risks
- Create clear, logical plans that downstream agents can execute
- Identify dependencies and ordering between items

Rules:
- Be systematic and thorough
- Use consistent categorization
- Flag any ambiguous items for review
- Output structured data that other agents can consume"#
            .into(),
        tools: vec!["list_repos".into()],
        provider: None,
        model: None,
        temperature: 0.2,
        max_tokens: 4096,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    }
}

pub fn researcher_config() -> AgentConfig {
    AgentConfig {
        id: "researcher".into(),
        name: "Researcher".into(),
        description: "Gathers and summarizes factual data from repositories and integrations.".into(),
        system_prompt: r#"You are a Researcher agent for a DevOps automation platform.

Your responsibilities:
- Gather data from source control systems (GitHub, GitLab) and from local repository clones (code search, files)
- For a single-repo task: use tool output to name impacted files, symbols, and concrete change suggestions
- Analyze commits, pull requests, merge requests, issues, and CI/CD pipelines when those tools are available
- Summarize findings in a structured, factual format
- Identify key changes, contributors, and patterns

Rules:
- Be thorough and factual — do not speculate beyond what tools show
- Include relevant metadata (paths, line hints, links)
- Group related items together
- Highlight high-impact changes"#
            .into(),
        tools: vec![
            "list_repos".into(),
            "code_search".into(),
            "read_file".into(),
            "list_tree".into(),
            "github_recent_activity".into(),
            "gitlab_recent_activity".into(),
            "github_merged_prs".into(),
            "gitlab_merged_prs".into(),
            "github_pipelines".into(),
            "gitlab_pipelines".into(),
        ],
        provider: None,
        model: None,
        temperature: 0.1,
        max_tokens: 8192,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    }
}

const WRITER_INSTRUCTIONS: &str = r#"You are a Writer agent for a DevOps automation platform.

Your responsibilities:
- Produce clear, professional written content from structured data
- Adapt tone and format to the content type (standup briefs, release notes, notifications)
- Be concise — stakeholders are busy
- Use consistent formatting and structure

For standup briefs:
- Lead with the most important items
- Use bullet points for clarity
- Include links where relevant
- Keep it under 500 words

For release notes:
- Use semantic versioning headers
- Group by category (Features, Fixes, Breaking Changes)
- Include PR/MR numbers and links
- Credit contributors

For deployment notifications:
- Lead with status (success/failure)
- Include what changed and who deployed
- Add relevant pipeline links"#;

const REVIEWER_INSTRUCTIONS: &str = r#"You are a Reviewer agent for a DevOps automation platform.

Your responsibilities:
- Review content produced by other agents for accuracy and completeness
- Verify that all important items from the source data are included
- Check formatting, grammar, and professional tone
- Provide a PASS or FAIL verdict with specific feedback

Output format:
- Start with "VERDICT: PASS" or "VERDICT: FAIL"
- If PASS: briefly note what looks good
- If FAIL: list specific issues that need fixing, each on its own line
- Be constructive and actionable in feedback"#;

const SYNTHESIZER_INSTRUCTIONS: &str = r#"You are a Synthesizer agent. Merge multi-repository research into one coherent change plan.
Explicitly call out ordering, shared APIs and schemas, event contracts, data dependencies, and risks.
Prefer structured JSON when the workflow requests it."#;

/// Default model used by the role agents when the operator/tenant catalog has
/// no override. The provider/model resolution chain (ADR-0012) overrides this
/// at request time.
const DEFAULT_ROLE_MODEL: &str = "openai/gpt-4o-mini";

#[must_use]
pub fn writer_code_agent(card_ctx: &CardEnrichmentContext, llm: Arc<dyn LlmProvider>) -> CodeAgent {
    CodeAgent::builder("writer")
        .description("Produces clear written content from structured inputs.")
        .instructions(WRITER_INSTRUCTIONS)
        .model(DEFAULT_ROLE_MODEL)
        .temperature(0.4)
        .max_tokens(4096)
        .card_context(card_ctx.clone())
        .llm(llm)
        .build()
        .expect("writer CodeAgent: required fields are set above")
}

#[must_use]
pub fn reviewer_code_agent(
    card_ctx: &CardEnrichmentContext,
    llm: Arc<dyn LlmProvider>,
) -> CodeAgent {
    CodeAgent::builder("reviewer")
        .description("Reviews agent output for quality and completeness.")
        .instructions(REVIEWER_INSTRUCTIONS)
        .model(DEFAULT_ROLE_MODEL)
        .temperature(0.1)
        .max_tokens(2048)
        .card_context(card_ctx.clone())
        .llm(llm)
        .build()
        .expect("reviewer CodeAgent: required fields are set above")
}

#[must_use]
pub fn synthesizer_code_agent(
    card_ctx: &CardEnrichmentContext,
    llm: Arc<dyn LlmProvider>,
) -> CodeAgent {
    CodeAgent::builder("synthesizer")
        .description("Merges multi-repository research into one coherent change plan.")
        .instructions(SYNTHESIZER_INSTRUCTIONS)
        .model(DEFAULT_ROLE_MODEL)
        .temperature(0.2)
        .max_tokens(8192)
        .card_context(card_ctx.clone())
        .llm(llm)
        .build()
        .expect("synthesizer CodeAgent: required fields are set above")
}

#[must_use]
pub fn seed_local_agents(
    card_ctx: &CardEnrichmentContext,
    llm: Arc<dyn LlmProvider>,
    tool_catalog: ToolCatalogBuilder,
) -> Vec<Arc<dyn Agent>> {
    vec![
        Arc::new(
            LocalAgent::new(planner_config(), card_ctx, llm.clone())
                .with_tool_catalog(tool_catalog.clone()),
        ),
        Arc::new(
            LocalAgent::new(researcher_config(), card_ctx, llm.clone())
                .with_tool_catalog(tool_catalog),
        ),
        Arc::new(writer_code_agent(card_ctx, llm.clone())),
        Arc::new(reviewer_code_agent(card_ctx, llm.clone())),
        Arc::new(synthesizer_code_agent(card_ctx, llm)),
    ]
}
