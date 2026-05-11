//! Two `CodeAgent`s wired to a real OpenAI-compatible LLM provider.
//!
//! - `concierge` is a chat-first agent that knows about the two demo
//!   tools (`clock-now`, `dice-roll`). Studio's Chat panel drives it.
//! - `analyst` is a structured-output agent that summarises the
//!   `daily-briefing` workflow result. v1 leaves it as a passive
//!   registration; the SPA's `Chat` panel currently hard-codes a
//!   single agent id (the post-Studio v2 ADR adds an agent picker).

use std::sync::Arc;

use anyhow::{Context, Result};
use ork_agents::CodeAgent;
use ork_common::config::ModelCapabilitiesEntry;
use ork_core::ports::llm::LlmProvider;
use ork_llm::openai_compatible::OpenAiCompatibleProvider;

use crate::tools;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Resolve the LLM provider from environment.
///
/// Priority:
/// 1. `ORK_DEMO_LLM_API_KEY` — opt-in, demo-specific.
/// 2. `OPENAI_API_KEY` — standard.
/// 3. `MINIMAX_API_KEY` — matches the existing kitchen-sink demo.
///
/// `ORK_DEMO_LLM_BASE_URL` / `ORK_DEMO_LLM_MODEL` override the
/// destination + model. The demo refuses to boot without a key so
/// reviewers don't accidentally get a 401 chain inside Studio.
pub fn build_llm_provider() -> Result<Arc<dyn LlmProvider>> {
    let (auth_header_name, auth_header_value, base_url, model) = resolve_credentials()?;

    let mut headers = std::collections::HashMap::new();
    headers.insert(auth_header_name, auth_header_value);

    let capabilities = vec![ModelCapabilitiesEntry {
        model: model.clone(),
        supports_tools: true,
        supports_streaming: true,
        supports_vision: false,
        max_context: Some(128_000),
    }];

    let provider = OpenAiCompatibleProvider::new(
        "demo-studio-tour-llm",
        base_url,
        Some(model),
        headers,
        capabilities,
    );
    Ok(Arc::new(provider) as Arc<dyn LlmProvider>)
}

fn resolve_credentials() -> Result<(String, String, String, String)> {
    let base_url =
        std::env::var("ORK_DEMO_LLM_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
    let model = std::env::var("ORK_DEMO_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());

    if let Ok(v) = std::env::var("ORK_DEMO_LLM_API_KEY") {
        return Ok(("Authorization".into(), normalise_bearer(&v), base_url, model));
    }
    if let Ok(v) = std::env::var("OPENAI_API_KEY") {
        return Ok(("Authorization".into(), normalise_bearer(&v), base_url, model));
    }
    if let Ok(v) = std::env::var("MINIMAX_API_KEY") {
        // ADR-0012 contract: MINIMAX_API_KEY is the literal header value;
        // the user may have already prefixed `Bearer `. Match the
        // kitchen-sink demo's interpretation.
        return Ok(("Authorization".into(), v, base_url, model));
    }
    Err(anyhow::anyhow!(
        "demo-studio-tour: set one of ORK_DEMO_LLM_API_KEY / OPENAI_API_KEY / MINIMAX_API_KEY \
         before booting. See README.md."
    ))
}

fn normalise_bearer(raw: &str) -> String {
    if raw.starts_with("Bearer ") {
        raw.to_string()
    } else {
        format!("Bearer {raw}")
    }
}

pub fn concierge_agent(llm: Arc<dyn LlmProvider>) -> Result<CodeAgent> {
    CodeAgent::builder("concierge")
        .description(
            "Friendly Studio-tour assistant. Knows the current time and how to roll dice.",
        )
        .instructions(
            "You are the ork Studio tour concierge. When the user asks for the time, call \
             `clock-now`. When they ask for a dice roll, call `dice-roll` with a sensible \
             `sides` and `count`. Otherwise, answer in one short paragraph. Keep responses \
             under 80 words.",
        )
        .model(format!(
            "{}/{}",
            "openai",
            std::env::var("ORK_DEMO_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into())
        ))
        .tool(tools::now_tool())
        .tool(tools::dice_tool())
        .llm(llm)
        .build()
        .context("build concierge agent")
}

pub fn analyst_agent(llm: Arc<dyn LlmProvider>) -> Result<CodeAgent> {
    CodeAgent::builder("analyst")
        .description("Compresses workflow output into a one-sentence headline.")
        .instructions(
            "You receive a `daily-briefing` workflow output. Reply with a single sentence: \
             `Headline: <something memorable>`. Do not call any tools.",
        )
        .model(format!(
            "{}/{}",
            "openai",
            std::env::var("ORK_DEMO_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into())
        ))
        .llm(llm)
        .build()
        .context("build analyst agent")
}
