# 0035 — Constrained decoding for tool calls

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0006, 0007, 0011, 0012, 0025, 0034, 0038
- **Supersedes:** —

## Context

ADR [`0011`](0011-native-llm-tool-calling.md) wires the agent loop in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
through `LlmProvider::chat_stream` with a tool catalog and trusts the
model to emit a syntactically valid `ToolCall.arguments` blob —
parsed at
[`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
into a `serde_json::Value` — that conforms to the tool's published
JSON Schema (`ToolDescriptor.parameters`). On a frontier hosted
model this trust is justified; on the weak local models that ADR
[`0034`](0034-per-model-capability-profiles.md) targets
(`qwen-2.5-coder-7b`, `llama-3-8b-instruct`) it is routinely
violated. Failure modes seen in dogfooding:

- Truncated JSON (closing brace missing) when the model hits its
  natural stop after a long argument list.
- Quoting drift (single quotes, unescaped newlines, trailing commas)
  on string fields.
- Schema-shape errors that *do* parse as JSON: an `integer` field
  arriving as `"42"`, an `enum` arriving with a value not in the
  allowed set.
- For the verifier path coming in ADR [`0038`] (plan
  cross-verification), a free-form prose verdict instead of the
  required `{ verdict, issues, ... }` payload — which makes
  aggregation across N verifiers impossible.

The agent loop currently catches these only at the boundary
(JSON parse failure, or tool-arg coercion failure when the
`ToolExecutor` rejects the shape) and surfaces an `OrkError` that
ends the loop. There is no in-loop repair mechanism that is *cheaper*
than another LLM round-trip.

Modern inference servers and a subset of hosted APIs eliminate this
class of failure at *decoding time* by constraining the sampler to
tokens that keep a partial output a valid prefix of the supplied
grammar / schema:

- **llama.cpp** — GBNF grammars via the `grammar` field on the
  completion endpoint.
- **vLLM** — `guided_json` / `guided_grammar` / `guided_choice`
  parameters.
- **TGI** — `grammar` parameter (JSON Schema or regex).
- **SGLang** — `regex` and `json_schema` constraints.
- **OpenAI-compatible** — `response_format = { type: "json_schema",
  json_schema: { schema, strict: true } }` (OpenAI itself, GPUStack
  per ADR [`0012`](0012-multi-llm-providers.md), Together, and
  Minimax exposing the same shape).

The agent loop already has the schemas it needs:

- For **tool calls**, `ToolDescriptor.parameters` is the JSON Schema
  the upstream tool source published (MCP server, native tool, peer
  agent card per ADR [`0011`](0011-native-llm-tool-calling.md) §
  Tool descriptor source).
- For **plan-verifier verdicts**, ADR [`0038`] (planned) defines a
  fixed `VerifierVerdict { verdict, score, issues, repair_hint }`
  schema that aggregation across cross-verifiers depends on.
- For ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  step-output schema — already declarable per workflow step in YAML
  — the same constraint mechanism applies when a producing step is
  pinned to a structured shape.

The capability gate sits on `ModelProfile.supports_grammar_constraint`
introduced by ADR [`0034`](0034-per-model-capability-profiles.md). What
0034 does **not** define is the wire shape, the trait extension on
`LlmProvider`, the per-provider rendering of "schema → grammar
field," or the in-loop retry policy when constrained decoding *still*
returns a non-conforming output (it can — providers fall back to
unconstrained decoding on grammar compile errors, and some adapters
silently truncate at `max_tokens`).

This ADR fills that gap.

## Decision

ork **introduces an optional constrained-decoding capability** on the
`LlmProvider` port, a `Constraint` payload carried on `ChatRequest`,
per-provider adapters that translate the payload into the wire-native
field, an in-loop retry policy with a typed exhaustion error, and a
clear separation of concerns between the *agent loop* (decides
*whether* to constrain a turn) and the *provider* (decides *how* to
render the constraint on the wire). Constrained decoding is gated on
the `ModelProfile.supports_grammar_constraint` flag from ADR
[`0034`](0034-per-model-capability-profiles.md); a provider that
cannot honour the constraint returns a typed
`ConstrainedDecodingUnsupported` error and the caller falls back to
unconstrained decoding.

### `ChatRequest` extension

```rust
// crates/ork-core/src/ports/llm.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// ADR 0035: optional sampling constraint. When `Some`, the
    /// provider MUST decode such that the response is a valid prefix
    /// of the constraint at every step; when the provider cannot
    /// honour the constraint it MUST return
    /// `OrkError::ConstrainedDecodingUnsupported`. The agent loop is
    /// responsible for setting this only when the resolved
    /// `ModelProfile.supports_grammar_constraint` is `true` and for
    /// stripping it otherwise (ADR 0034 §`(d)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<Constraint>,
}

/// What the response must conform to. JSON Schema is the lingua
/// franca; provider adapters translate it to whatever the wire wants
/// (GBNF, `guided_json`, `response_format=json_schema`, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Constraint {
    /// Constrain the assistant's *final* message text to a JSON
    /// document matching `schema`. Used for verifier verdicts (ADR
    /// 0038) and ADR 0025's structured step outputs. `name` is a
    /// stable handle (e.g. `"verifier_verdict"`,
    /// `"invoice_extraction"`) the provider may forward as the
    /// schema name when the wire requires one (OpenAI's
    /// `response_format.json_schema.name`).
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        /// When `true`, the provider MUST reject any token that
        /// would lead to an unrecoverable schema violation. When
        /// `false`, the provider MAY treat the schema as a hint
        /// (currently used only by adapters whose servers don't
        /// expose a strict mode).
        #[serde(default = "Constraint::strict_default")]
        strict: bool,
    },
    /// Constrain the model to choose one of the *named* tool calls
    /// in `ChatRequest.tools` and to emit `arguments` matching that
    /// tool's `parameters` schema. Used when the agent loop is
    /// confident the model's next turn must be a tool call (e.g.
    /// after pinning `tool_choice = Required` on a weak model).
    /// Mutually exclusive with `JsonSchema`.
    ToolCall {
        /// Subset of `ChatRequest.tools` (by name) the model may
        /// emit. An empty vec means "any tool in `tools`".
        allowed: Vec<String>,
    },
    /// Raw GBNF grammar. Escape hatch for agents that need a
    /// non-JSON shape (rare). Not derivable from a JSON Schema; the
    /// agent must supply it directly. Adapters whose servers don't
    /// accept GBNF return `ConstrainedDecodingUnsupported` for this
    /// variant.
    Gbnf { grammar: String },
}

impl Constraint {
    fn strict_default() -> bool { true }
}
```

The new field is additive and `skip_serializing_if = "Option::is_none"`
so persisted `ChatRequest` history from before this ADR deserialises
unchanged. The existing `ChatRequest::simple` constructor stays
backwards-compatible; a new
`ChatRequest::with_constraint(constraint: Constraint) -> Self`
chainable setter is added for ergonomic call-site construction.

### `LlmProvider` extension

```rust
// crates/ork-core/src/ports/llm.rs (continued)

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OrkError>;
    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError>;
    fn provider_name(&self) -> &str;
    fn capabilities(&self, _model: &str) -> ModelCapabilities { ModelCapabilities::default() }
    async fn capabilities_for(&self, request: &ChatRequest) -> ModelCapabilities {
        let model = request.model.as_deref().unwrap_or("");
        self.capabilities(model)
    }

    /// ADR 0035: declare whether this provider can honour a
    /// `Constraint` for the resolved `(provider, model)` pair. The
    /// default impl returns `false` — a provider that *can* constrain
    /// must override this to return `true` for the relevant models.
    /// Routers (ADR 0012's `LlmRouter`) override this to delegate to
    /// the resolved underlying provider.
    fn supports_constraint(&self, _model: &str, _constraint: &Constraint) -> bool { false }
}
```

`chat`/`chat_stream` keep one signature: `request.constraint`
travels in-band. A provider that receives `Some(constraint)` while
`supports_constraint(...)` would have returned `false` MUST return
`OrkError::ConstrainedDecodingUnsupported { provider, model, kind }`
*before* opening the upstream stream — never silently drop the
constraint. The `kind` field carries which `Constraint` variant was
asked for so the caller's fallback can log a precise reason.

### Schema sources (no duplicate authoring)

The agent loop derives the JSON Schema from one of three existing
surfaces — never authored locally:

1. **Tool catalog (ADR [`0011`](0011-native-llm-tool-calling.md)).**
   `ToolDescriptor.parameters` is already JSON Schema. When the loop
   pins `tool_choice = Required` on a weak model and only one tool
   is allowed, it attaches `Constraint::ToolCall { allowed: vec![the
   tool name] }`; provider adapters render this as a per-tool schema
   from the descriptor.
2. **Verifier verdict (ADR [`0038`], planned).** ADR [`0038`]
   exposes the verdict schema via a single helper
   `VerifierVerdict::json_schema()` returning a
   `&'static serde_json::Value`. The `LocalAgent` plan-verifier path
   wraps this in `Constraint::JsonSchema { name:
   "verifier_verdict", schema, strict: true }`.
3. **Step output (ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)).**
   When a workflow step's `validate.schema` is set and the agent's
   profile supports constraints, the engine attaches
   `Constraint::JsonSchema { name: step_id, schema, strict: true }`
   for the producing turn. Stage-1 schema check still runs on the
   returned content as a defence-in-depth guard against adapter
   bugs.

This ADR introduces no schema authoring of its own. Any future
fixed-shape payload (e.g. a topology-classifier verdict per ADR
[`0026`](0026-workflow-topology-selection-from-task-features.md))
follows the same pattern: the owning ADR defines the schema; this
ADR provides only the wire path.

### Per-provider adapters

`ork-llm` gains a small `constraint` module that translates a
`Constraint` to the per-server wire field. Each provider impl
delegates to it on the way out:

```rust
// crates/ork-llm/src/constraint.rs

pub(crate) enum WireConstraint {
    /// llama.cpp `grammar` field; GBNF text.
    LlamaCppGbnf(String),
    /// vLLM `guided_json` / `guided_choice` / `guided_grammar`.
    VllmGuided(VllmGuided),
    /// OpenAI-compatible `response_format = { type: "json_schema",
    /// json_schema: { name, schema, strict } }`.
    OpenAiJsonSchema { name: String, schema: serde_json::Value, strict: bool },
}

pub(crate) enum VllmGuided {
    Json { schema: serde_json::Value },
    Grammar { grammar: String },
}

pub(crate) fn render_for_llama_cpp(c: &Constraint, tools: &[ToolDescriptor])
    -> Result<WireConstraint, OrkError>;

pub(crate) fn render_for_vllm(c: &Constraint, tools: &[ToolDescriptor])
    -> Result<WireConstraint, OrkError>;

pub(crate) fn render_for_openai_compatible(c: &Constraint, tools: &[ToolDescriptor])
    -> Result<WireConstraint, OrkError>;
```

Render rules per variant:

| `Constraint` variant | llama.cpp (GBNF) | vLLM (`guided_*`) | OpenAI-compat (`response_format`) |
| --- | --- | --- | --- |
| `JsonSchema { schema, strict }` | compile schema → GBNF via the `gbnf-from-json-schema` helper crate (vendored, see Out of scope) | `guided_json = schema` | `response_format = { type: "json_schema", json_schema: { name, schema, strict } }` |
| `ToolCall { allowed }` | union of per-tool schemas (one rule per allowed name); rejects when `allowed` is empty *and* `tools` is empty | `guided_json = { oneOf: [per-tool schemas] }` | `response_format = { type: "json_schema", json_schema: { schema: { oneOf: [per-tool schemas] }, strict: true } }` plus `tool_choice = Required` |
| `Gbnf { grammar }` | passthrough | `guided_grammar = grammar` (vLLM accepts GBNF) | `Err(ConstrainedDecodingUnsupported)` — OpenAI-compat has no GBNF route |

The `ToolCall` rendering deliberately threads through `tools`: the
adapter already has the catalog from `ChatRequest.tools` and does
not need a second copy of the schemas. Adapters that lack any
guided-decoding support (e.g. a hypothetical legacy provider) keep
the default `supports_constraint = false` and never enter
`render_for_*`.

The minimum required adapter set for v1 is:

- llama.cpp (GBNF) at `crates/ork-llm/src/llama_cpp.rs` (new).
- vLLM (`guided_json`) at `crates/ork-llm/src/vllm.rs` (new).
- OpenAI-compatible (`response_format`) extends the existing
  [`crates/ork-llm/src/openai_compatible.rs`](../../crates/ork-llm/src/openai_compatible.rs).

TGI and SGLang are deferred (Open question below); their wire
shapes are similar enough that adding them is a follow-up of one
file each.

### `LocalAgent` integration

`LocalAgent::send_stream` (ADR
[`0011`](0011-native-llm-tool-calling.md)) gains one decision point
per turn, immediately before building the `ChatRequest`:

1. Resolve the effective `ModelProfile` (ADR
   [`0034`](0034-per-model-capability-profiles.md) §`LocalAgent
   integration` step (d)). If
   `profile.supports_grammar_constraint == false`, set
   `request.constraint = None` and emit
   `tracing::debug!(target = "ork.constraint", reason =
   "profile_disabled")`.
2. Else, consult the *constraint policy* for this turn:
   - **Plan-verifier turn (ADR 0038):** attach
     `Constraint::JsonSchema { name: "verifier_verdict", schema:
     VerifierVerdict::json_schema().clone(), strict: true }`.
   - **Step with declared output schema (ADR 0025):** attach
     `Constraint::JsonSchema { name: step.id.clone(), schema:
     step.validate.schema.clone(), strict: true }`.
   - **Forced-tool turn:** when the loop pinned `tool_choice =
     Required` and `allowed.len() == 1`, attach
     `Constraint::ToolCall { allowed: vec![the tool name] }`.
   - **Otherwise:** `None`. Free-form prose turns are *never*
     constrained — see Out of scope.
3. An agent that wants to override the policy for a creative,
   non-tool turn (e.g. an "explain the plan to the user" turn after
   plan acceptance) sets `agent_config.force_unconstrained =
   true` for that step; the policy short-circuits to `None`. This
   override lives on the `WorkflowStep` (ADR
   [`0018`](0018-dag-executor-enhancements.md)'s `extras`) and on
   `AgentConfig`; no new wire field on the A2A surface is added.

### In-loop retry policy

A provider may legitimately return content that fails JSON-Schema
validation even with `strict: true` — the canonical case is a
constraint compile-time error that the server silently degrades to
unconstrained, but truncation at `max_tokens` and adapter bugs also
qualify. The loop must therefore validate the response *after*
constrained decoding and retry with a small, deterministic budget.

```text
budget = 1 retry per turn (default), bounded by
         max_constraint_retries (config, default 1)
on validation failure k of the turn's response:
  history.push(assistant_message_as_received)        // for transparency
  history.push(system_message: "The previous response did not
               match the required schema. Validation error: <err>.
               Return only the JSON document conforming to the
               schema.")
  re-issue ChatRequest with the SAME constraint
exhaustion (k == budget):
  return OrkError::ConstrainedDecodingExhausted {
      provider, model, schema_name, last_error
  }
```

The retry is single-shot by default. ADR
[`0038`] (plan cross-verification) treats a verifier whose response
exhausts this retry as `verdict = request_changes` with `issues =
[{ kind: "verifier_unparseable", message: <last_error> }]` so a
malformed verifier never silently passes nor crashes the run; the
behaviour is owned by ADR [`0038`] but the *signal* (the typed
`ConstrainedDecodingExhausted` error) is owned here.

The new `OrkError` variants live in
[`crates/ork-common/src/error.rs`](../../crates/ork-common/src/error.rs):

```rust
#[error("constrained decoding unsupported: provider={provider} model={model} kind={kind}")]
ConstrainedDecodingUnsupported {
    provider: String,
    model: String,
    kind: &'static str, // "json_schema" | "tool_call" | "gbnf"
},

#[error("constrained decoding retries exhausted after {attempts}: {schema_name}: {last_error}")]
ConstrainedDecodingExhausted {
    provider: String,
    model: String,
    schema_name: String,
    attempts: u32,
    last_error: String,
},
```

Both are `Configuration`/`Validation`-class errors (see
[`crates/ork-common/src/error.rs`](../../crates/ork-common/src/error.rs)
for the existing taxonomy); they are non-retryable from the engine's
perspective once raised, so the workflow engine surfaces them via
the existing failure path without further repair.

### Mesh interaction (ADR 0006 / 0007)

Constrained decoding is **agent-local**. When `LocalAgent` delegates
to a peer via `agent_call` (ADR
[`0006`](0006-peer-delegation.md)) or to a remote A2A agent (ADR
[`0007`](0007-remote-a2a-agent-client.md)), the constraint travels
*nowhere on the wire* — it stays inside the producer's loop. The
only thing the orchestrator sees is the verified output (or the
typed exhaustion error). Concretely:

- The A2A `message/send` and `message/stream` payloads
  ([`crates/ork-a2a`](../../crates/ork-a2a/)) gain *no* new field.
  Whether a peer constrains its own decoding is its private
  decision, governed by the peer's own `ModelProfile`.
- The peer's agent card extension from ADR
  [`0034`](0034-per-model-capability-profiles.md)
  (`https://ork.dev/a2a/extensions/model-profile`) already advertises
  `supports_grammar_constraint`. Discovery (ADR [`0042`], planned)
  may use this when ranking which peer to delegate a strict-schema
  task to, but the orchestrator does not negotiate a constraint
  with the peer — it just picks a peer whose profile says yes.
- For ADR [`0038`]'s cross-verification, the orchestrator sends the
  plan to N verifiers; each verifier's `LocalAgent` independently
  decides to constrain its own verdict. Aggregation reads the
  verdicts as parsed JSON, not as constrained tokens.

This is deliberate: ADR
[`0034`](0034-per-model-capability-profiles.md) §`Multi-tenant
override surface` already requires that profile decisions stay local
to the tenant; surfacing a per-peer constraint negotiation on the
wire would violate that boundary and create a new vector for cross-
tenant inference.

### Configuration surface

Operator config (`crates/ork-common/src/config.rs`) gains a
`[llm.constraint]` block:

```toml
[llm.constraint]
# Hard cap on the in-loop retry budget per turn. 0 disables retry.
max_retries_per_turn = 1
# When a provider returns ConstrainedDecodingUnsupported, fall back
# to unconstrained decoding (true) or fail the turn (false). Default
# true; tenant config can flip to false for environments that demand
# strict-shape contracts (compliance, regulated outputs).
fallback_when_unsupported = true
```

Tenant override (per ADR
[`0020`](0020-tenant-security-and-trust.md)'s tenant config
pipeline) layers in `[constraint] fallback_when_unsupported =
false` for tenants that opt out of unconstrained fallback. No
runtime admin API; restart-on-change matches every other tenant
setting.

### Out of scope

- **Constrained prose.** Constraining the *natural-language* portion
  of a response (e.g. forcing a markdown structure on a chat reply)
  is not addressed. JSON Schema is the lingua franca for *structured*
  output; constraining prose with a grammar is a separate problem
  (citation enforcement, formatting) deferred to a future ADR.
- **A grammar authoring DSL.** Operators do not write GBNF or Lark
  grammars in ork config. JSON Schema is the only authored shape;
  GBNF/Lark are produced *from* a schema by adapters. The `Gbnf`
  variant on `Constraint` is an escape hatch for agent code that
  computes a grammar programmatically (rare).
- **Schema evolution / versioning of the verifier verdict.** Owned
  by ADR [`0038`]; this ADR consumes whatever
  `VerifierVerdict::json_schema()` returns at compile time.
- **The schema → GBNF compiler.** A small helper module (vendored
  or pulled from a permissive crate; the implementing session
  picks) turns a JSON Schema into GBNF for the llama.cpp adapter.
  Coverage is the JSON-Schema subset ork actually emits (the same
  subset MCP servers and OpenAI's `strict: true` mode permit:
  `type`, `properties`, `required`, `enum`, `items`, `oneOf`,
  `anyOf`, `pattern` for strings — *no* `$ref` cycles, *no*
  unbounded recursion). Failures to compile a schema fall back to
  `ConstrainedDecodingUnsupported` and the agent loop retries
  without a constraint when `fallback_when_unsupported = true`.
- **Streaming partial JSON to clients.** ADR
  [`0011`](0011-native-llm-tool-calling.md)'s
  `ChatStreamEvent::ToolCallDelta` already exposes partial JSON for
  tool calls; constrained decoding does not change that surface.
- **Token-budget accounting for constrained outputs.** Constrained
  decoding sometimes uses more tokens than unconstrained (the
  sampler is forced to longer paths). Cost telemetry remains owned
  by ADR [`0022`](0022-observability.md); we add the `schema_name`
  label to the existing `ork_llm_*` metrics so the cost of strict
  decoding is attributable per-schema.
- **Top-level grammar caching.** A reasonable optimisation
  (compile schema once, reuse across turns) is left to the
  implementing session as an internal concern of `ork-llm`.

## Acceptance criteria

- [ ] Type `Constraint` defined at
      [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
      with the three variants in `Decision` (`JsonSchema`,
      `ToolCall`, `Gbnf`) and `#[serde(rename_all = "snake_case", tag
      = "kind")]`; derives `Clone + Debug + Serialize + Deserialize`.
- [ ] `Constraint::strict_default()` returns `true`;
      round-trip serde test
      `crates/ork-core/tests/llm_constraint_serde.rs::roundtrip`
      asserts the example payload deserialises and re-serialises
      byte-stable for each variant.
- [ ] `ChatRequest` gains a `constraint: Option<Constraint>` field
      with `#[serde(default, skip_serializing_if = "Option::is_none")]`;
      `ChatRequest::simple` keeps its current signature and sets
      `constraint = None`; `ChatRequest::with_constraint` chainable
      setter is added.
- [ ] `LlmProvider::supports_constraint(&self, model: &str,
      constraint: &Constraint) -> bool` added to the trait at
      [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
      with a default impl returning `false`.
- [ ] `OrkError::ConstrainedDecodingUnsupported { provider, model,
      kind }` and `OrkError::ConstrainedDecodingExhausted { provider,
      model, schema_name, attempts, last_error }` defined at
      [`crates/ork-common/src/error.rs`](../../crates/ork-common/src/error.rs).
- [ ] Module `crates/ork-llm/src/constraint.rs` defines
      `WireConstraint`, `VllmGuided`, and three rendering functions
      (`render_for_llama_cpp`, `render_for_vllm`,
      `render_for_openai_compatible`) with the mapping rules from
      `Decision`.
- [ ] llama.cpp adapter at `crates/ork-llm/src/llama_cpp.rs`
      implements `LlmProvider` with a working `chat_stream` against a
      stub HTTP server in
      `crates/ork-llm/tests/llama_cpp_grammar.rs::sends_grammar_field`
      that asserts the outgoing request body contains the GBNF
      `grammar` field rendered from a `Constraint::JsonSchema`.
- [ ] vLLM adapter at `crates/ork-llm/src/vllm.rs` implements
      `LlmProvider` with a working `chat_stream` against a stub HTTP
      server in `crates/ork-llm/tests/vllm_guided.rs::sends_guided_json`
      asserting the outgoing request carries `guided_json` populated
      from the schema.
- [ ] OpenAI-compatible adapter at
      [`crates/ork-llm/src/openai_compatible.rs`](../../crates/ork-llm/src/openai_compatible.rs)
      attaches `response_format = { type: "json_schema", json_schema:
      { name, schema, strict } }` when `request.constraint =
      Constraint::JsonSchema { strict: true, .. }`; verified by
      `crates/ork-llm/tests/openai_response_format.rs::sends_response_format`.
- [ ] Each provider's `supports_constraint` returns `true` for
      `JsonSchema` and `ToolCall` variants and (for OpenAI-compat
      only) `false` for the `Gbnf` variant; verified by
      `crates/ork-llm/tests/{llama_cpp,vllm,openai}_supports.rs`
      smoke tests.
- [ ] When a provider receives a `Constraint` it cannot honour, it
      returns `OrkError::ConstrainedDecodingUnsupported { .. }` with
      `kind` set to `"json_schema" | "tool_call" | "gbnf"` per the
      variant; verified by
      `crates/ork-llm/tests/openai_response_format.rs::rejects_gbnf`.
- [ ] In-loop retry policy: integration test
      `crates/ork-agents/tests/constraint_retry.rs::single_retry_then_exhausts`
      installs a stub provider that returns malformed JSON twice,
      configures `max_retries_per_turn = 1`, and asserts
      `LocalAgent::send_stream` yields exactly one retry with a
      `system` repair message in history and then an
      `OrkError::ConstrainedDecodingExhausted { attempts: 2, .. }`.
- [ ] Profile gate: integration test
      `crates/ork-agents/tests/constraint_profile_gate.rs::strips_when_profile_disables`
      sets `ModelProfile.supports_grammar_constraint = false`,
      builds a request with
      `constraint = Some(Constraint::JsonSchema { .. })`, and
      asserts the constraint is `None` on the request reaching the
      stub provider; a `tracing::debug!(target = "ork.constraint",
      reason = "profile_disabled")` event is emitted.
- [ ] Config: `[llm.constraint]` block parsed by
      [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs)
      with `max_retries_per_turn: u32` (default `1`) and
      `fallback_when_unsupported: bool` (default `true`); tenant
      override via the ADR
      [`0020`](0020-tenant-security-and-trust.md) loader verified by
      `crates/ork-common/tests/constraint_config.rs::tenant_overrides_fallback`.
- [ ] Fallback policy: integration test
      `crates/ork-agents/tests/constraint_fallback.rs::falls_back_when_unsupported`
      stubs a provider that returns `ConstrainedDecodingUnsupported`
      for `JsonSchema`, sets `fallback_when_unsupported = true`, and
      asserts the agent loop re-issues the same request with
      `constraint = None` and succeeds; with `false`, the same setup
      surfaces `ConstrainedDecodingUnsupported` to the caller.
- [ ] Mesh boundary: assertion test
      `crates/ork-a2a/tests/no_constraint_on_wire.rs::message_send_carries_no_constraint`
      asserts that no field named `constraint`, `grammar`,
      `guided_json`, or `response_format` is serialised on any A2A
      JSON-RPC message envelope produced by `ork-a2a`.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0035`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- Weak local models (`qwen-2.5-coder-7b`, `llama-3-8b-instruct` per
  ADR [`0034`](0034-per-model-capability-profiles.md)) become
  *usable* for tool-calling agent loops: the JSON-shape failure
  mode that dominates today is removed at decoding time, not after.
- ADR [`0038`]'s plan cross-verification becomes mechanical:
  verdicts are guaranteed parseable, aggregation across N verifiers
  is `serde_json::from_str` + count, no LLM-as-judge for "did the
  judge respond correctly."
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  Stage-1 schema check becomes a defence-in-depth safety net rather
  than the primary gate; the median producing-step's output already
  conforms when handed back, so the repair loop runs less often
  (cost & latency win).
- Provider plurality stays cheap: adding a fourth or fifth server
  is one file under `crates/ork-llm/src/` plus one render arm in
  `constraint.rs`. The agent loop and the schema sources don't
  change.
- The "tenant decides whether unsupported = fatal" knob lets
  compliance-flavoured deployments enforce strict shapes without
  forcing the rest of ork into the same posture.

### Negative / costs

- A new wire-shaped enum on the public `LlmProvider` port. Every
  current and future provider has to know about `Constraint`, even
  if only to return `false` from `supports_constraint`. We accept
  this — the alternative (a separate trait) splits the call site
  and makes routing harder. The default impls keep additions cheap.
- Schema → GBNF compilation is non-trivial; bugs in the compiler
  manifest as a *silent* fall-through to unconstrained decoding
  (when the upstream server compiles the grammar to nothing) or as
  surprising rejections of valid output. Mitigated by: (a) keeping
  the supported subset narrow and documented; (b) a fallback path
  governed by `fallback_when_unsupported`; (c) the post-decode
  validation step that catches silent fall-through and triggers
  the retry.
- Constrained decoding can use *more* tokens than unconstrained,
  occasionally a lot more (the sampler is forced through longer
  paths to satisfy the grammar). ADR
  [`0022`](0022-observability.md) gains a `schema_name` label so
  operators can spot pathological costs; we accept the small
  measurement burden.
- Provider drift: each server's guided-decoding implementation has
  bugs, version skew, and silent regressions. ork's adapters will
  age — we will discover that `vLLM 0.x` accepts `guided_json` but
  `0.y` requires `response_format`. Mitigated by: per-adapter
  smoke tests against a stub server; the typed
  `ConstrainedDecodingUnsupported` error surfaces drift clearly
  rather than as malformed JSON.
- The `Gbnf` variant is an escape hatch that complicates the trait
  and the OpenAI-compat adapter (which must reject it). We accept
  it because llama.cpp users with non-JSON shapes (e.g. constrained
  CSV) would otherwise vendor their own forks. The criterion that
  OpenAI-compat returns `ConstrainedDecodingUnsupported` for
  `Gbnf` keeps the contract honest.
- Agent-loop coupling: the loop now consults the profile *and* the
  constraint policy *and* the retry budget per turn. Three knobs
  per turn is more than ADR
  [`0011`](0011-native-llm-tool-calling.md) had. The acceptance
  criteria pin the ordering (profile → policy → retry) so the
  precedence is testable, not folkloric.
- Strict mode on OpenAI's `response_format=json_schema` rejects
  schemas with `additionalProperties: true` and a few other
  features. Schemas authored by tool-source ADRs (0011's MCP-
  imported schemas, 0025's user-authored YAML schemas) may fail to
  pass strict mode. Mitigated by: when `strict = true` round-trip
  fails to compile at the provider, the adapter returns
  `ConstrainedDecodingUnsupported { kind: "json_schema" }` and the
  loop falls back per `fallback_when_unsupported`; we *do not*
  silently re-render with `strict: false`.

### Neutral / follow-ups

- TGI and SGLang adapters are deferred; both are file-sized
  follow-ups and follow the same pattern. The Open question section
  notes them.
- A constrained-prose ADR (citation-enforcement, markdown structure)
  is a plausible future ADR; it would *consume* the same trait but
  add new `Constraint` variants. The `tag = "kind"` shape is
  forward-compatible.
- ADR [`0042`] (planned) discovery may add a `constraint_dialects`
  hint to the agent card extension when more than one dialect is
  observed in the wild. Today the boolean
  `supports_grammar_constraint` is sufficient.
- A future ADR may add observability for `constraint_satisfied`
  vs. `constraint_violated` per turn (i.e. whether constrained
  decoding *actually* prevented a violation, vs. whether the model
  would have produced valid JSON anyway). Useful for measuring the
  ROI of constrained decoding per model.
- Schema → GBNF compiler maintenance: track upstream fixes; if a
  permissive crate (e.g. `gbnf-from-json-schema`) becomes well-
  maintained, switch to a dependency. For v1 we vendor.

## Alternatives considered

- **Tool-arg validation only, no constrained decoding.** The agent
  loop already parses `ToolCall.arguments` as JSON and the
  `ToolExecutor` validates against the tool's schema; just retry on
  failure. Rejected: the retry is *another full LLM round-trip*,
  it's expensive, and weak local models often fail in the same way
  on the second attempt. Constrained decoding fixes the issue at
  source for ~no incremental cost.
- **A separate `ConstrainedLlmProvider` trait.** Cleaner in one
  sense (the base trait stays unchanged). Rejected: routing
  becomes harder — `LlmRouter` has to multiplex two trait objects,
  and the agent loop has to fallback-switch on which one resolved.
  Putting the capability on the existing trait, gated by the
  `supports_constraint` declaration and the typed
  `ConstrainedDecodingUnsupported` error, keeps the call site
  uniform.
- **Render constraints to GBNF universally and route every provider
  through the GBNF wire.** Rejected: vLLM and OpenAI-compatible
  servers expose richer JSON-Schema-aware modes; downgrading them
  to GBNF loses native features (e.g. OpenAI's `strict: true`
  rejection of malformed schema sub-features) and doubles the
  schema-compilation surface to maintain.
- **Author a custom grammar DSL in YAML/TOML.** Operators write
  `validate.grammar:` directly. Rejected: violates "no duplicate
  schema authoring." Every relevant schema already exists as JSON
  Schema (MCP tool, verifier verdict, step output). A second DSL
  is a footgun.
- **Negotiate constrained decoding on the A2A wire (per peer).**
  Add a `constraint` field on A2A `message/send` and let
  orchestrators ask remote agents to constrain their output.
  Rejected: violates ADR
  [`0034`](0034-per-model-capability-profiles.md)'s tenant
  boundary on profile decisions and creates a cross-tenant
  inference vector. Each peer decides locally based on its own
  profile; the orchestrator only sees the result.
- **Move the retry policy to the provider impl, not the agent
  loop.** Symmetric with how some HTTP clients handle 429s.
  Rejected: the *content* of the repair message ("the previous
  response did not match the schema") is agent-loop-shaped — it
  goes into chat history. Providers don't have a chat-history
  abstraction; they ship one request and read one stream. The
  retry must live one layer up.
- **Drop the `Gbnf` variant.** Simpler trait, simpler provider
  matrix. Rejected: agents that need non-JSON constrained shapes
  (constrained CSV, constrained code with tag matching) would
  vendor their own. The `Gbnf` variant is small (one render arm
  per adapter, one rejection arm for OpenAI-compat) and keeps the
  escape hatch in-tree.
- **Skip constrained decoding entirely; lean on the validation
  gate from ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md) plus
  the repair loop.** Rejected: the repair loop's per-iteration cost
  is one full LLM call; on a weak local model the repair often
  fails the same way. Constrained decoding eliminates the failure
  rather than retrying through it. ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md) and
  this ADR are complementary: 0025 catches what slips through;
  this ADR makes "what slips through" rare.
- **Force `strict: true` always.** Cleaner semantics. Rejected: a
  non-trivial fraction of MCP-imported schemas fail OpenAI's
  strict-mode subset checks; allowing `strict: false` per
  `Constraint::JsonSchema` keeps those tools usable on hosted
  providers (the loop still post-validates).

## Affected ork modules

- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs) —
  `Constraint` enum, `ChatRequest.constraint`, trait method
  `supports_constraint`.
- [`crates/ork-common/src/error.rs`](../../crates/ork-common/src/error.rs) —
  `ConstrainedDecodingUnsupported` and
  `ConstrainedDecodingExhausted` variants on `OrkError`.
- New: `crates/ork-llm/src/constraint.rs` — render functions,
  `WireConstraint`, the schema → GBNF helper (vendored or
  dep-tracked).
- New: `crates/ork-llm/src/llama_cpp.rs` — adapter for llama.cpp
  servers; consumes `render_for_llama_cpp`.
- New: `crates/ork-llm/src/vllm.rs` — adapter for vLLM servers;
  consumes `render_for_vllm`.
- [`crates/ork-llm/src/openai_compatible.rs`](../../crates/ork-llm/src/openai_compatible.rs) —
  adds `response_format = json_schema` rendering;
  `supports_constraint` returns `true` for `JsonSchema` /
  `ToolCall` and `false` for `Gbnf`.
- [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs) —
  `supports_constraint` delegation to the resolved provider; no
  behavioural change beyond the new method.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) —
  constraint policy decision per turn (profile gate → policy →
  retry); fallback per `fallback_when_unsupported`.
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) —
  `[llm.constraint]` block (`max_retries_per_turn`,
  `fallback_when_unsupported`) and tenant override path.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) —
  attaches `Constraint::JsonSchema` for steps with declared
  `validate.schema` (ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md));
  no other behavioural change.
- [`docs/adrs/README.md`](README.md) — ADR index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on
the implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step
3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| llama.cpp | `grammar` field on `/completion` (GBNF) | `Constraint::Gbnf` + `render_for_llama_cpp` |
| vLLM | `guided_json` / `guided_grammar` / `guided_choice` parameters | `Constraint::JsonSchema` + `render_for_vllm` |
| TGI | `grammar` parameter (JSON Schema or regex) | (deferred, see Open questions) |
| SGLang | `regex` and `json_schema` constraints | (deferred, see Open questions) |
| OpenAI | `response_format = { type: "json_schema", json_schema: { name, schema, strict } }` | `Constraint::JsonSchema` + `render_for_openai_compatible` |
| Outlines / lm-format-enforcer | "constrained decoding" library category | The Rust analogue, scoped to the providers ork already wires |
| Aider | `-strict` JSON repair loop after the fact | This ADR's in-loop retry, but with constrained decoding doing most of the work first |

## Open questions

- **TGI and SGLang adapters.** Both have wire shapes close to
  vLLM's. Stance: add as one-file follow-ups when a deployment
  needs them; track in a follow-up ADR or as a routine `ork-llm`
  task.
- **Schema → GBNF compiler dependency.** Vendor a small in-tree
  compiler vs. depend on a permissive crate. Stance: vendor v1 to
  control the supported subset; switch to a dep when one becomes
  well-maintained.
- **Constrained decoding for assistant text deltas vs. final
  message.** Some servers stream constrained tokens as deltas;
  others only validate at the end. ork consumes
  `ChatStreamEvent::Delta` as opaque text today. Stance: leave
  delta semantics unchanged; clients render whatever the wire
  produces, post-decode validation runs on the aggregated final
  message.
- **`strict = false` re-render.** When `strict = true` fails to
  compile at the provider, should the adapter automatically retry
  with `strict = false` before returning
  `ConstrainedDecodingUnsupported`? Stance: no for v1 — silent
  loosening of the contract is a footgun. The `fallback_when_
  unsupported` knob is the explicit lever.
- **MCP-imported schema strict-mode compatibility.** Some MCP
  servers publish schemas that violate OpenAI's strict subset
  (e.g. `additionalProperties: true` implicit). Stance: track the
  failure rate via the `schema_name` metric label; if material,
  add a normalising pass on import (separate ADR).
- **Per-step constraint override on the A2A wire.** A future ADR
  may want a workflow author to declare "this step's verdict must
  be constrained even though the agent's profile says no." Stance:
  defer; the current `WorkflowStep.extras` plus
  `force_unconstrained` lever covers the inverse direction
  (force-off), which is the more common request.
- **Caching compiled grammars across runs.** A reasonable
  optimisation but risks staleness if a schema changes. Stance:
  implementation detail of `ork-llm`, not a contract.

## References

- llama.cpp grammar (GBNF):
  <https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md>
- vLLM structured outputs:
  <https://docs.vllm.ai/en/latest/usage/structured_outputs.html>
- TGI guidance:
  <https://huggingface.co/docs/text-generation-inference/conceptual/guidance>
- SGLang structured generation:
  <https://docs.sglang.ai/sampling_params.html>
- OpenAI structured outputs:
  <https://platform.openai.com/docs/guides/structured-outputs>
- Outlines:
  <https://github.com/dottxt-ai/outlines>
- A2A spec — extensions: <https://github.com/google/a2a>
- Related ADRs:
  [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0011`](0011-native-llm-tool-calling.md),
  [`0012`](0012-multi-llm-providers.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0034`](0034-per-model-capability-profiles.md),
  0038 (forthcoming).
