use std::sync::Arc;

use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::llm::LlmProvider;
use ork_core::workflow::engine::ToolExecutor;

use crate::roles::seed_local_agents;
use crate::tool_catalog::ToolCatalogBuilder;

#[must_use]
pub fn build_default_registry(
    card_ctx: &CardEnrichmentContext,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolExecutor>,
) -> AgentRegistry {
    AgentRegistry::from_agents(seed_local_agents(
        card_ctx,
        llm,
        tools,
        ToolCatalogBuilder::new(),
    ))
}

#[must_use]
pub fn build_default_registry_with_catalog(
    card_ctx: &CardEnrichmentContext,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolExecutor>,
    tool_catalog: ToolCatalogBuilder,
) -> AgentRegistry {
    AgentRegistry::from_agents(seed_local_agents(card_ctx, llm, tools, tool_catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalAgent;
    use crate::roles::{planner_config, researcher_config};
    use ork_core::a2a::AgentContext;
    use ork_core::ports::agent::Agent;
    use ork_core::ports::llm::{ChatRequest, ChatStreamEvent, FinishReason, LlmProvider};
    use ork_core::workflow::engine::ToolExecutor;

    struct StubLlm;

    #[async_trait::async_trait]
    impl LlmProvider for StubLlm {
        async fn chat(
            &self,
            _request: ChatRequest,
        ) -> Result<ork_core::ports::llm::ChatResponse, ork_common::error::OrkError> {
            unreachable!()
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<ork_core::ports::llm::LlmChatStream, ork_common::error::OrkError> {
            let s = async_stream::stream! {
                yield Ok(ChatStreamEvent::Delta("x".into()));
                yield Ok(ChatStreamEvent::Done {
                    usage: ork_core::ports::llm::TokenUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    },
                    model: "stub".into(),
                    finish_reason: FinishReason::Stop,
                });
            };
            Ok(Box::pin(s))
        }

        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    struct StubTools;

    #[async_trait::async_trait]
    impl ToolExecutor for StubTools {
        async fn execute(
            &self,
            _ctx: &AgentContext,
            _tool_name: &str,
            _input: &serde_json::Value,
        ) -> Result<serde_json::Value, ork_common::error::OrkError> {
            Ok(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn default_registry_resolves_planner_and_lists_cards() {
        let ctx = CardEnrichmentContext::minimal();
        let reg = build_default_registry(&ctx, Arc::new(StubLlm), Arc::new(StubTools));
        assert!(reg.resolve(&"planner".to_string()).await.is_some());
        assert!(reg.resolve(&"synthesizer".to_string()).await.is_some());
        assert_eq!(reg.list_cards().await.len(), 5);
    }

    #[tokio::test]
    async fn from_two_local_agents() {
        let ctx = CardEnrichmentContext::minimal();
        let llm: Arc<dyn LlmProvider> = Arc::new(StubLlm);
        let tools: Arc<dyn ToolExecutor> = Arc::new(StubTools);
        let a1 = Arc::new(LocalAgent::new(
            planner_config(),
            &ctx,
            llm.clone(),
            tools.clone(),
        )) as Arc<dyn Agent>;
        let a2 = Arc::new(LocalAgent::new(researcher_config(), &ctx, llm, tools)) as Arc<dyn Agent>;
        let reg = AgentRegistry::from_agents([a1, a2]);
        assert_eq!(reg.list_cards().await.len(), 2);
        assert!(reg.resolve(&"planner".to_string()).await.is_some());
        assert!(reg.resolve(&"researcher".to_string()).await.is_some());
    }
}
