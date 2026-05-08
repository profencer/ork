//! [`CodeAgent`] / [`CodeAgentBuilder`] — code-first agent DSL
//! (ADR [`0052`](../../docs/adrs/0052-code-first-agent-dsl.md)).
//!
//! The builder produces a value implementing the
//! [`Agent`](ork_core::ports::agent::Agent) port (ADR [`0002`](../../docs/adrs/0002-agent-port.md)),
//! preserving the A2A surface (cards, tasks, streaming, cancel) for free. The
//! engine driver underneath is the rig adapter shipped by ADR
//! [`0047`](../../docs/adrs/0047-rig-as-local-agent-engine.md).
//!
//! `CodeAgent` is the *primary* authoring path; [`crate::local::LocalAgent`] remains as
//! a low-level escape hatch for callers who need bespoke behaviour outside the
//! builder shape (e.g., custom history seeding).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use futures::StreamExt as _;
use ork_a2a::{
    AgentCard, AgentSkill, Message as AgentMessage, Part, TaskEvent as AgentEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::a2a::{AgentContext, AgentId, ResolveContext};
use ork_core::models::agent::{
    AgentConfig, default_max_parallel_tool_calls, default_max_tool_iterations,
    default_max_tool_result_bytes,
};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::llm::{ChatMessage, ChatRequest, LlmProvider};
use ork_core::ports::tool_def::ToolDef;
use ork_tool::IntoToolDef;

use crate::hooks::{CompletionHook, ToolHook};
use crate::instruction_spec::InstructionSpec;
use crate::model_spec::ModelSpec;
use crate::rig_engine::{OutputSlot, RigEngine, RigEngineHooks};

/// Closure resolving the system prompt from a per-request [`AgentContext`].
/// Mirrors Mastra's [`dynamic-agents`](https://mastra.ai/docs/agents/dynamic-agents)
/// shape — useful for per-tenant role injection or feature-flag prompting.
pub type DynamicInstructionsFn = Arc<
    dyn Fn(&AgentContext) -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync + 'static,
>;

/// Closure resolving a [`ModelSpec`] from a per-request [`AgentContext`].
/// Used for per-tenant model switching (e.g. Premium → `gpt-4o`, Standard → `gpt-4o-mini`).
pub type DynamicModelFn = Arc<
    dyn Fn(&AgentContext) -> Pin<Box<dyn Future<Output = ModelSpec> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Closure producing additional [`ToolDef`] entries for a single request, on
/// top of the static [`CodeAgentBuilder::tool`] list. Synchronous because tool
/// resolution is typically a registry lookup, not an I/O operation.
pub type DynamicToolsFn =
    Arc<dyn Fn(&AgentContext) -> Vec<Arc<dyn ToolDef>> + Send + Sync + 'static>;

/// Code-first agent built from [`CodeAgent::builder`]. Implements the `Agent` port.
pub struct CodeAgent {
    id: AgentId,
    card: AgentCard,
    config: AgentConfig,
    llm: Arc<dyn LlmProvider>,
    tools: Vec<Arc<dyn ToolDef>>,
    agent_refs: Vec<String>,
    workflow_refs: Vec<String>,
    mcp_server_refs: Vec<String>,
    dyn_instructions: Option<DynamicInstructionsFn>,
    dyn_model: Option<DynamicModelFn>,
    dyn_tools: Option<DynamicToolsFn>,
    tool_hooks: Vec<Arc<dyn ToolHook>>,
    completion_hooks: Vec<Arc<dyn CompletionHook>>,
    output_schema: Option<serde_json::Value>,
    request_context_schema: Option<serde_json::Value>,
}

impl CodeAgent {
    /// JSON Schema declared via [`CodeAgentBuilder::request_context_schema`].
    /// Consumed by ADR-0056's auto-OpenAPI emitter and ADR-0055 Studio's "send
    /// a message" form. Returns `None` when no schema was set.
    #[must_use]
    pub fn request_context_schema(&self) -> Option<&serde_json::Value> {
        self.request_context_schema.as_ref()
    }
}

impl std::fmt::Debug for CodeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeAgent")
            .field("id", &self.id)
            .field(
                "tools",
                &self.tools.iter().map(|t| t.id()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl CodeAgent {
    /// Begin building a `CodeAgent`. The id is non-empty; required fields are
    /// `instructions`, `model`, and `llm`.
    #[must_use]
    pub fn builder(id: impl Into<String>) -> CodeAgentBuilder {
        CodeAgentBuilder {
            id: id.into(),
            ..CodeAgentBuilder::empty()
        }
    }
}

/// Builder for [`CodeAgent`]. Phase 1 carries the static authoring shape: id,
/// description, instructions, model, temperature/limits, and a flat tool list.
/// Dynamic resolvers, hooks, sub-agent / workflow tools, and structured outputs
/// land in later phases.
pub struct CodeAgentBuilder {
    id: AgentId,
    description: Option<String>,
    skills: Option<Vec<AgentSkill>>,
    instructions: Option<InstructionSpec>,
    model: Option<ModelSpec>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    max_steps: Option<usize>,
    max_parallel_tool_calls: Option<usize>,
    max_tool_result_bytes: Option<usize>,
    expose_reasoning: bool,
    tools: Vec<Arc<dyn ToolDef>>,
    agent_refs: Vec<String>,
    workflow_refs: Vec<String>,
    mcp_server_refs: Vec<String>,
    dyn_instructions: Option<DynamicInstructionsFn>,
    dyn_model: Option<DynamicModelFn>,
    dyn_tools: Option<DynamicToolsFn>,
    tool_hooks: Vec<Arc<dyn ToolHook>>,
    completion_hooks: Vec<Arc<dyn CompletionHook>>,
    output_schema: Option<serde_json::Value>,
    request_context_schema: Option<serde_json::Value>,
    card_ctx: Option<CardEnrichmentContext>,
    llm: Option<Arc<dyn LlmProvider>>,
}

impl CodeAgentBuilder {
    fn empty() -> Self {
        Self {
            id: String::new(),
            description: None,
            skills: None,
            instructions: None,
            model: None,
            temperature: None,
            max_tokens: None,
            max_steps: None,
            max_parallel_tool_calls: None,
            max_tool_result_bytes: None,
            expose_reasoning: false,
            tools: Vec::new(),
            agent_refs: Vec::new(),
            workflow_refs: Vec::new(),
            mcp_server_refs: Vec::new(),
            dyn_instructions: None,
            dyn_model: None,
            dyn_tools: None,
            tool_hooks: Vec::new(),
            completion_hooks: Vec::new(),
            output_schema: None,
            request_context_schema: None,
            card_ctx: None,
            llm: None,
        }
    }

    /// Set the agent description (placed in the [`AgentCard`] and the default skill).
    #[must_use]
    pub fn description(mut self, s: impl Into<String>) -> Self {
        self.description = Some(s.into());
        self
    }

    /// Override the auto-derived [`AgentSkill`] list. The default is one skill per
    /// agent: `id = "{agent_id}-default"`, `name = agent_id`, `description = description`.
    #[must_use]
    pub fn skills(mut self, s: Vec<AgentSkill>) -> Self {
        self.skills = Some(s);
        self
    }

    /// Static system prompt. Required (or [`Self::dynamic_instructions`] in Phase 3).
    #[must_use]
    pub fn instructions(mut self, s: impl Into<InstructionSpec>) -> Self {
        self.instructions = Some(s.into());
        self
    }

    /// Static model selection (`"provider/model"` or bare model name). Required
    /// (or `dynamic_model` in Phase 3). Resolution chains through ADR-0012's
    /// [`LlmRouter`](ork_llm) — per-step overrides still apply at request time.
    #[must_use]
    pub fn model(mut self, s: impl Into<ModelSpec>) -> Self {
        self.model = Some(s.into());
        self
    }

    #[must_use]
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    #[must_use]
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// Maximum tool-loop iterations (rig multi-turn cap). Defaults to
    /// [`default_max_tool_iterations`].
    #[must_use]
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }

    #[must_use]
    pub fn max_parallel_tool_calls(mut self, n: usize) -> Self {
        self.max_parallel_tool_calls = Some(n);
        self
    }

    #[must_use]
    pub fn max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = Some(n);
        self
    }

    #[must_use]
    pub fn expose_reasoning(mut self, on: bool) -> Self {
        self.expose_reasoning = on;
        self
    }

    /// Register a typed tool (ADR-0051 [`IntoToolDef`]).
    #[must_use]
    pub fn tool<T: IntoToolDef>(mut self, t: T) -> Self {
        self.tools.push(t.into_tool_def());
        self
    }

    /// Register a type-erased tool (e.g. an [`Arc<dyn ToolDef>`] from a registry).
    #[must_use]
    pub fn tool_dyn(mut self, t: Arc<dyn ToolDef>) -> Self {
        self.tools.push(t);
        self
    }

    /// Resolve the system prompt at request entry from the per-request
    /// [`AgentContext`]. Mutually exclusive with [`Self::instructions`] in the
    /// sense that whichever is set last wins; both satisfy the
    /// "instructions required" build-time check.
    ///
    /// Mastra parity:
    /// [dynamic-agents](https://mastra.ai/docs/agents/dynamic-agents).
    #[must_use]
    pub fn dynamic_instructions<F>(mut self, f: F) -> Self
    where
        F: Fn(&AgentContext) -> Pin<Box<dyn Future<Output = String> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.dyn_instructions = Some(Arc::new(f));
        self
    }

    /// Resolve the [`ModelSpec`] at request entry. Per-step LLM overrides from
    /// the workflow engine still win at the call site (ADR-0012 §`Selection`).
    #[must_use]
    pub fn dynamic_model<F>(mut self, f: F) -> Self
    where
        F: Fn(&AgentContext) -> Pin<Box<dyn Future<Output = ModelSpec> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.dyn_model = Some(Arc::new(f));
        self
    }

    /// Inject additional tools at request entry on top of the static list.
    /// Useful for per-tenant tool gating that depends on the caller / scopes.
    #[must_use]
    pub fn dynamic_tools<F>(mut self, f: F) -> Self
    where
        F: Fn(&AgentContext) -> Vec<Arc<dyn ToolDef>> + Send + Sync + 'static,
    {
        self.dyn_tools = Some(Arc::new(f));
        self
    }

    /// Declare the JSON Schema that incoming request bodies must satisfy
    /// (ADR-0052 §`request_context_schema`). The schema is exposed via
    /// [`CodeAgent::request_context_schema`] for ADR-0056's OpenAPI emitter
    /// and ADR-0055 Studio's request form.
    ///
    /// Runtime body validation is a follow-up — it lands when the auto-REST
    /// surface (ADR-0056) is wired. The bounds match the ADR's Decision
    /// section to signal intent at the type level even when only `JsonSchema`
    /// is consumed by `schemars::schema_for!`.
    #[must_use]
    pub fn request_context_schema<C>(mut self) -> Self
    where
        C: schemars::JsonSchema + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        // schemars::RootSchema → serde_json::Value never fails for well-formed
        // `JsonSchema` impls; `expect` over `unwrap_or` surfaces a derive bug
        // loudly instead of silently shipping a `null` schema downstream.
        let schema = serde_json::to_value(schemars::schema_for!(C))
            .expect("schemars::RootSchema must serialize to JSON");
        self.request_context_schema = Some(schema);
        self
    }

    /// Switch the agent into *extractor mode* (ADR-0052 §`Structured output via
    /// rig::Extractor`). At request time a synthetic `submit` tool is injected
    /// whose input schema is `schemars::schema_for!(O)`; the agent's terminal
    /// [`AgentMessage`] carries one [`Part::Data`] with the validated value.
    ///
    /// Per the implementation plan ([`docs/adrs/0052-code-first-agent-dsl.md`])
    /// the value lands as a `Data` part rather than a synthetic
    /// `AgentEvent::Output` variant, since `TaskEvent` has no such variant
    /// today and Studio/A2A clients already render `Data` parts.
    ///
    /// The `DeserializeOwned + Send + Sync + 'static` bounds match the ADR
    /// Decision section: callers that round-trip the captured `Value` into
    /// `O` (for example, a future `OrkApp::run_agent_typed::<O>`) need them.
    #[must_use]
    pub fn output_schema<O>(mut self) -> Self
    where
        O: schemars::JsonSchema + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let schema = serde_json::to_value(schemars::schema_for!(O))
            .expect("schemars::RootSchema must serialize to JSON");
        self.output_schema = Some(schema);
        self
    }

    /// Attach a [`ToolHook`] (ADR-0052 §`Hooks`). Multiple hooks chain in
    /// registration order; the first non-`Proceed` decision short-circuits the
    /// invocation. Mastra parity: `inputProcessors` / runtime tool gating.
    #[must_use]
    pub fn on_tool_call<H: ToolHook + 'static>(mut self, h: H) -> Self {
        self.tool_hooks.push(Arc::new(h));
        self
    }

    /// Attach a [`CompletionHook`] fired with the agent's terminal text. Used
    /// by ADR-0058 observability and ADR-0054 live scorers.
    #[must_use]
    pub fn on_completion<H: CompletionHook + 'static>(mut self, h: H) -> Self {
        self.completion_hooks.push(Arc::new(h));
        self
    }

    /// Reference an MCP server registered on the same [`OrkApp`](ork_app::OrkApp).
    /// Validated at `OrkApp::build()`. Runtime expansion of MCP-derived tools is
    /// a follow-up — Phase 2 captures the dependency for cross-ref checking only.
    #[must_use]
    pub fn tool_server(mut self, mcp_server_id: impl Into<String>) -> Self {
        self.mcp_server_refs.push(mcp_server_id.into());
        self
    }

    /// Reference a peer agent registered on the same [`OrkApp`](ork_app::OrkApp).
    /// Validated at `OrkApp::build()` — order on the builder chain is irrelevant
    /// (forward refs are allowed; the topological check happens once the app is
    /// finalised).
    ///
    /// Mastra parity: collapses Mastra's `agents` parameter into a typed verb.
    #[must_use]
    pub fn agent_as_tool(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_refs.push(agent_id.into());
        self
    }

    /// Reference a workflow registered on the same [`OrkApp`](ork_app::OrkApp).
    /// Validated at `OrkApp::build()`. Runtime dispatch through `run_workflow`
    /// (ADR-0050) is a follow-up.
    #[must_use]
    pub fn workflow_as_tool(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_refs.push(workflow_id.into());
        self
    }

    /// Override the [`AgentCard`] enrichment context. The default
    /// [`CardEnrichmentContext::minimal`] suits unit tests and the bootless dev path.
    #[must_use]
    pub fn card_context(mut self, ctx: CardEnrichmentContext) -> Self {
        self.card_ctx = Some(ctx);
        self
    }

    /// Wire the LLM provider used by [`RigEngine`]. Required.
    ///
    /// `OrkAppBuilder` injects the operator/tenant `LlmRouter` here at registration time;
    /// stand-alone tests pass a scripted `LlmProvider` directly.
    #[must_use]
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Validate required fields and produce a [`CodeAgent`].
    ///
    /// Returns [`OrkError::Configuration`] when `id` is empty, or `instructions`,
    /// `model`, or `llm` were not set.
    pub fn build(self) -> Result<CodeAgent, OrkError> {
        if self.id.trim().is_empty() {
            return Err(OrkError::Configuration {
                message: "CodeAgent: id must not be empty (call `CodeAgent::builder(\"my-id\")`)"
                    .into(),
            });
        }

        let id = self.id.clone();
        let missing = |field: &str, hint: &str| OrkError::Configuration {
            message: format!("CodeAgent `{id}`: {field} is required ({hint})"),
        };

        if self.instructions.is_none() && self.dyn_instructions.is_none() {
            return Err(missing(
                "instructions",
                "call .instructions(\"...\") or .dynamic_instructions(...) before .build()",
            ));
        }
        if self.model.is_none() && self.dyn_model.is_none() {
            return Err(missing(
                "model",
                "call .model(\"provider/model\") or .dynamic_model(...) before .build()",
            ));
        }
        let llm = self.llm.ok_or_else(|| {
            missing(
                "llm",
                "call .llm(provider) before .build() (OrkAppBuilder injects this automatically)",
            )
        })?;

        let description = self.description.unwrap_or_default();
        // Static fallback used when the dynamic resolver is not set; the resolver,
        // when present, overrides this at request time.
        let system_prompt = match self.instructions {
            Some(InstructionSpec::Static(s)) => s,
            None => String::new(),
        };
        let static_model = self.model.unwrap_or_default();

        // CodeAgent passes its tool list directly to RigEngine; the allow-list logic in
        // `ToolCatalogBuilder` (used by LocalAgent) is bypassed because the user already
        // chose the tools at build time.
        let tool_ids: Vec<String> = self.tools.iter().map(|t| t.id().to_string()).collect();

        let config = AgentConfig {
            id: id.clone(),
            name: id.clone(),
            description: description.clone(),
            system_prompt,
            tools: tool_ids,
            provider: static_model.provider.clone(),
            model: static_model.model.clone(),
            temperature: self.temperature.unwrap_or(0.2),
            max_tokens: self.max_tokens.unwrap_or(2048),
            max_tool_iterations: self.max_steps.unwrap_or_else(default_max_tool_iterations),
            max_parallel_tool_calls: self
                .max_parallel_tool_calls
                .unwrap_or_else(default_max_parallel_tool_calls),
            max_tool_result_bytes: self
                .max_tool_result_bytes
                .unwrap_or_else(default_max_tool_result_bytes),
            expose_reasoning: self.expose_reasoning,
        };

        let card_ctx = self.card_ctx.unwrap_or_default();
        let mut card = build_local_card(&config, &card_ctx);
        if let Some(skills) = self.skills {
            card.skills = skills;
        }

        // Dedupe ref lists deterministically so cross-ref reporting and any
        // OpenAPI-style emitters see stable order without surprising the caller.
        let agent_refs = dedupe_preserve_order(self.agent_refs);
        let workflow_refs = dedupe_preserve_order(self.workflow_refs);
        let mcp_server_refs = dedupe_preserve_order(self.mcp_server_refs);

        Ok(CodeAgent {
            id,
            card,
            config,
            llm,
            tools: self.tools,
            agent_refs,
            workflow_refs,
            mcp_server_refs,
            dyn_instructions: self.dyn_instructions,
            dyn_model: self.dyn_model,
            dyn_tools: self.dyn_tools,
            tool_hooks: self.tool_hooks,
            completion_hooks: self.completion_hooks,
            output_schema: self.output_schema,
            request_context_schema: self.request_context_schema,
        })
    }
}

/// System-prompt suffix appended in extractor mode. Steers the LLM to call the
/// synthetic `submit` tool with O-shaped arguments and stop, instead of
/// producing free-form text.
const EXTRACTOR_INSTRUCTION: &str = "\n\nReply by calling the `submit` tool with arguments that match the required schema. Do not produce free-form text after that call.";

/// Synthetic tool injected by [`CodeAgent`] when `output_schema::<O>()` is set.
/// On invocation it stashes the parsed args into the request-scoped
/// [`OutputSlot`] and returns an empty ack so the rig loop can terminate.
struct SubmitTool {
    schema: serde_json::Value,
    output_schema: serde_json::Value,
    slot: OutputSlot,
}

#[async_trait]
impl ToolDef for SubmitTool {
    fn id(&self) -> &str {
        "submit"
    }
    fn description(&self) -> &str {
        "Submit the structured output for this task."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn output_schema(&self) -> &serde_json::Value {
        &self.output_schema
    }
    async fn invoke(
        &self,
        _ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        if let Ok(mut g) = self.slot.lock() {
            *g = Some(input.clone());
        }
        Ok(serde_json::json!({"ok": true}))
    }
}

fn dedupe_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn extract_prompt_text(msg: &AgentMessage) -> Result<String, OrkError> {
    let mut s = String::new();
    for p in &msg.parts {
        match p {
            Part::Text { text, .. } => s.push_str(text),
            Part::Data { data, .. } => {
                s.push_str(&serde_json::to_string(data).unwrap_or_default());
            }
            Part::File { .. } => {
                return Err(OrkError::Validation(
                    "file parts are not supported in CodeAgent yet (TODO ADR-0003/0016)".into(),
                ));
            }
        }
    }
    if s.is_empty() {
        return Err(OrkError::Validation(
            "agent message has no usable text content".into(),
        ));
    }
    Ok(s)
}

#[async_trait]
impl Agent for CodeAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn card(&self) -> &AgentCard {
        &self.card
    }

    fn referenced_agent_ids(&self) -> &[String] {
        &self.agent_refs
    }

    fn referenced_workflow_ids(&self) -> &[String] {
        &self.workflow_refs
    }

    fn referenced_mcp_server_ids(&self) -> &[String] {
        &self.mcp_server_refs
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let prompt = extract_prompt_text(&msg)?;
        let task_id = ctx.task_id;
        let mut user = ChatMessage::user(prompt);
        user.parts = msg.parts.clone();

        let mut config = self.config.clone();

        // ADR-0052 §`Dynamic instructions, model, tools` — resolve per-request
        // values *before* the LLM call. Static values stand in when no
        // resolver is set. Workflow-step overrides (ADR-0012) still win at the
        // call site below.
        if let Some(f) = self.dyn_instructions.as_ref() {
            config.system_prompt = f(&ctx).await;
        }
        if let Some(f) = self.dyn_model.as_ref() {
            let spec = f(&ctx).await;
            config.provider = spec.provider;
            config.model = spec.model;
        }

        let mut tools = self.tools.clone();
        if let Some(f) = self.dyn_tools.as_ref() {
            tools.extend(f(&ctx));
        }

        // ADR-0052 §`Structured output via rig::Extractor` — when the agent is
        // in extractor mode, inject the synthetic `submit` tool, augment the
        // system prompt, and pass an OutputSlot to the engine so the consumer
        // emits a `Part::Data` terminal Message instead of free-form text.
        let extractor_slot: Option<OutputSlot> = if let Some(schema) = self.output_schema.as_ref() {
            let slot: OutputSlot = Arc::new(std::sync::Mutex::new(None));
            let submit = Arc::new(SubmitTool {
                schema: schema.clone(),
                output_schema: serde_json::json!({"type":"object"}),
                slot: slot.clone(),
            });
            tools.push(submit as Arc<dyn ToolDef>);
            config.system_prompt.push_str(EXTRACTOR_INSTRUCTION);
            Some(slot)
        } else {
            None
        };

        // Re-derive the visible tool ids in case the dynamic resolver added entries; the
        // value is mostly used for telemetry/manifests, not the rig dispatch path.
        config.tools = tools.iter().map(|t| t.id().to_string()).collect();

        let llm = self.llm.clone();
        let tool_hooks = self.tool_hooks.clone();
        let completion_hooks = self.completion_hooks.clone();
        let resolve_ctx = ResolveContext {
            tenant_id: ctx.tenant_id,
        };

        // ADR-0012 §`Selection` — workflow-step overrides win over per-agent defaults.
        let step_overrides = ctx.step_llm_overrides.clone();
        let request_provider = step_overrides
            .as_ref()
            .and_then(|o| o.provider.clone())
            .or_else(|| config.provider.clone());
        let request_model = step_overrides
            .as_ref()
            .and_then(|o| o.model.clone())
            .or_else(|| config.model.clone());

        let s = stream! {
            yield Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus { state: TaskState::Working, message: None },
                is_final: false,
            }));

            let preflight = ChatRequest {
                messages: Vec::new(),
                temperature: None,
                max_tokens: None,
                model: request_model.clone(),
                provider: request_provider.clone(),
                tools: Vec::new(),
                tool_choice: None,
            };
            let caps = resolve_ctx.scope(llm.capabilities_for(&preflight)).await;
            if !tools.is_empty() && !caps.supports_tools {
                let label = request_model.clone().unwrap_or_default();
                yield Err(OrkError::LlmProvider(format!(
                    "model {label} does not support tool calls"
                )));
                return;
            }

            if ctx.cancel.is_cancelled() {
                yield Err(OrkError::Workflow("agent task cancelled".into()));
                return;
            }

            match RigEngine::run(
                ctx.clone(),
                config.clone(),
                llm.clone(),
                tools,
                user.clone(),
                vec![],
                RigEngineHooks {
                    tool: tool_hooks.clone(),
                    completion: completion_hooks.clone(),
                    extractor_slot: extractor_slot.clone(),
                },
            )
            .await
            {
                Err(e) => {
                    yield Err(e);
                }
                Ok(mut inner) => {
                    while let Some(ev) = inner.next().await {
                        yield ev;
                    }
                }
            }
        };

        Ok(Box::pin(s) as AgentEventStream)
    }
}
