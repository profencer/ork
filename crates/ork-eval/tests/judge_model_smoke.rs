//! ADR-0054 acceptance criterion `LLM-as-judge scorers`
//! ([live-scorers ADR](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).
//!
//! Verifies the LLM-as-judge contract end-to-end against a scripted
//! `LlmProvider` that returns a fixed structured `(score, rationale)`
//! response. The scorer must:
//! - call the judge model exactly once,
//! - parse the response into `JudgeOutput`,
//! - surface it on the resulting `ScoreCard`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity, TaskId};
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage,
};
use ork_core::ports::scorer::{RunId, RunKind, ScoreInput, Trace};
use ork_eval::scorers::{LlmProviderJudge, answer_relevancy, faithfulness, toxicity};
use tokio_util::sync::CancellationToken;

/// Scripted LLM mirroring the pattern in
/// `crates/ork-agents/tests/code_agent_extractor.rs`. Returns the
/// next pre-scripted reply on each `chat` call.
struct ScriptedLlm {
    replies: Mutex<Vec<String>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedLlm {
    fn new(replies: Vec<&str>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(String::from).collect()),
            requests: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OrkError> {
        self.requests.lock().unwrap().push(request);
        let next = self
            .replies
            .lock()
            .unwrap()
            .pop()
            .expect("scripted reply exhausted");
        Ok(ChatResponse {
            content: next,
            model: "scripted".into(),
            usage: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
        })
    }

    async fn chat_stream(&self, _request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let s: LlmChatStream = Box::pin(stream::empty::<Result<ChatStreamEvent, OrkError>>());
        Ok(s)
    }

    fn provider_name(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn ctx() -> AgentContext {
    AgentContext {
        tenant_id: TenantId(uuid::Uuid::nil()),
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: TenantId(uuid::Uuid::nil()),
            user_id: None,
            scopes: vec![],
            tenant_chain: vec![TenantId(uuid::Uuid::nil())],
            trust_tier: TrustTier::Internal,
            trust_class: TrustClass::User,
            agent_id: None,
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

fn empty_trace(user: &str) -> Trace {
    Trace {
        user_message: user.into(),
        tool_calls: vec![],
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn answer_relevancy_parses_judge_output_and_surfaces_score() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        r#"{"score": 0.9, "rationale": "directly answers the question"}"#,
    ]));
    let judge = Arc::new(LlmProviderJudge::new(llm.clone(), "openai/gpt-4o-mini"));
    let scorer = answer_relevancy().judge(judge).build();
    let ctx = ctx();
    let trace = empty_trace("how warm is SF?");
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "how warm is SF?",
            final_response: "It's 70F and sunny.",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert!((card.score - 0.9).abs() < 1e-5);
    assert!(card.rationale.as_deref().unwrap().contains("directly"));
    assert_eq!(card.details["judge_model"], "openai/gpt-4o-mini");

    // The judge prompt must include both the user message and the response.
    let req = llm.requests.lock().unwrap()[0].clone();
    let prompt_text = &req.messages[0].content;
    assert!(prompt_text.contains("how warm is SF?"));
    assert!(prompt_text.contains("It's 70F and sunny."));
}

#[tokio::test]
async fn faithfulness_includes_tool_call_context_in_prompt() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        r#"{"score": 0.8, "rationale": "all claims grounded"}"#,
    ]));
    let judge = Arc::new(LlmProviderJudge::new(llm.clone(), "openai/gpt-4o-mini"));
    let scorer = faithfulness().judge(judge).build();
    let ctx = ctx();
    let trace = Trace {
        user_message: "u".into(),
        tool_calls: vec![ork_core::ports::scorer::ToolCallRecord {
            name: "get_weather".into(),
            args: serde_json::json!({"city": "SF"}),
            result: serde_json::json!({"temp_f": 70}),
            duration_ms: 10,
            error: None,
        }],
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
    };
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "70F in SF",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert!((card.score - 0.8).abs() < 1e-5);
    let req = llm.requests.lock().unwrap()[0].clone();
    let prompt_text = &req.messages[0].content;
    assert!(prompt_text.contains("get_weather"));
}

#[tokio::test]
async fn toxicity_inverts_judge_score() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        r#"{"score": 0.2, "rationale": "mostly benign"}"#,
    ]));
    let judge = Arc::new(LlmProviderJudge::new(llm, "openai/gpt-4o-mini"));
    let scorer = toxicity().judge(judge).build();
    let ctx = ctx();
    let trace = empty_trace("u");
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: "have a nice day",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    // Judge said 0.2 toxic → scorer reports 0.8 (1.0 - 0.2).
    assert!((card.score - 0.8).abs() < 1e-5);
    assert!((card.details["raw_toxicity"].as_f64().unwrap() - 0.2).abs() < 1e-5);
}

#[tokio::test]
async fn fenced_json_replies_still_parse() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        "```json\n{\"score\": 1.0, \"rationale\": \"perfect\"}\n```",
    ]));
    let judge = Arc::new(LlmProviderJudge::new(llm, "openai/gpt-4o-mini"));
    let scorer = answer_relevancy().judge(judge).build();
    let ctx = ctx();
    let trace = empty_trace("u");
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: "r",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
}
