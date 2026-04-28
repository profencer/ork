# 0034 — Per-model capability profiles

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0005, 0011, 0012, 0020, 0022, 0029, 0032, 0033, 0035, 0038, 0042
- **Supersedes:** —

## Context

The same `LocalAgent` loop in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
must drive an agent on top of a frontier hosted model
(`gpt-5`, `claude-sonnet-4-6`) and a weak local model
(`llama-3-8b-instruct`, `qwen-2.5-coder-7b` running on a single GPU
behind GPUStack per ADR [`0012`](0012-multi-llm-providers.md)). These
two regimes need radically different agent behaviour:

- **Edit format.** Frontier models reliably emit unified-diff hunks; a
  7B local model only succeeds on whole-file rewrites or Aider-style
  search/replace blocks. ADR
  [`0029`](0029-workspace-file-editor.md) introduces the file editor's
  three formats but leaves the *selection* of one to the caller.
- **Tool-catalog cap.** Tool selection accuracy collapses for many
  local models past ~8 native tools, while frontier models cope with
  30+. ADR [`0011`](0011-native-llm-tool-calling.md) gates on
  `ModelCapabilities::supports_tools` (a boolean) but has no notion of
  "this model can route over `n` tools, drop the optional ones."
- **Compaction threshold.** ADR
  [`0032`](0032-agent-memory-and-context-compaction.md) fires
  compaction at `caps.max_context * compaction_trigger_ratio`. The
  ratio is uniform across models, but practical compaction thresholds
  are not — a 200K-context model that degrades sharply past 60K and a
  32K model that degrades gracefully both deserve a per-model
  *absolute* token threshold.
- **Default temperature.** Reasoning-tuned models want `0.0`–`0.3`;
  general chat models want `0.7`. The agent loop currently passes
  whatever the request carries.
- **Grammar-constrained decoding.** Some local servers
  (vLLM, llama.cpp, GPUStack) accept grammar / JSON-Schema-constrained
  decoding; hosted providers usually do not. ADR [`0035`] (planned)
  introduces the grammar surface but cannot land usefully without a
  per-model "is this safe to send" signal.
- **Native tool-call support.** Already partly present as
  `ModelCapabilities::supports_tools` at
  [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs);
  needs to be carried as the same advisory bit on the profile so the
  loop's decisions all consult one shape, not two.
- **Thinking mode.** Reasoning models (`gpt-5-thinking`,
  `claude-sonnet-4-6` with extended thinking, DeepSeek-R1) accept and
  benefit from "think first, then answer" prompts; non-reasoning
  models do not. The right policy is per-model and often
  per-phase: think during Plan (ADR [`0038`]) but not during Execute.

Two role-aware properties are layered on top:

- **Multi-agent teams.** ADR [`0045`] (planned) composes architect,
  executor, reviewer, tester roles. Each persona (per ADR
  [`0033`](0033-coding-agent-personas.md)) runs on a different model.
  Profiles must therefore be addressable per persona and per agent —
  not as a single global "the model is X." ADR
  [`0033`]'s `ModelProfileRef` is the consumer; this ADR is the
  thing it refers to.
- **Plan cross-verification.** ADR [`0038`] (planned) requires that the
  plan verifier run on a *different* model from the planner to avoid
  same-model echo. When the workflow does not name a verifier
  explicitly, ADR [`0038`]'s gate has to pick one; profiles surface
  that hint as `recommended_plan_verifier_model_id`.

Today's surface that nearly does this — but does not — is
[`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)'s
`ModelCapabilities` (four fields: `supports_tools`,
`supports_streaming`, `supports_vision`, `max_context`) and its
operator-facing twin `ModelCapabilitiesEntry` at
[`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs).
That surface answers "what does the wire support" — a compatibility
gate. This ADR adds a parallel surface that answers "how should the
agent loop *behave* when running on this model" — a tuning gate.
The two stay distinct: one wrong `supports_tools` value loses
correctness; one wrong `compaction_threshold_tokens` value loses
performance.

## Decision

ork **introduces** a `ModelProfile` descriptor in `ork-llm`, a
`ModelProfileRegistry` for in-process lookup, an override chain
honouring ADR [`0020`](0020-tenant-security-and-trust.md)'s tenant
boundary, and an A2A agent-card extension that publishes the subset
of the profile needed by ADR [`0042`]'s capability discovery. The
profile is **advisory** — not a wire-level guarantee — and lives next
to but separate from `ModelCapabilities`. `LocalAgent` consults the
profile at six well-defined decision points; everything else stays
unchanged.

### `ModelProfile` (ork-llm)

```rust
// crates/ork-llm/src/profile.rs

use std::sync::Arc;

/// Behavioural tuning for a single model. Separate from
/// `ModelCapabilities` (wire flags); both are consulted by the agent
/// loop, but a wrong field here is a performance regression rather
/// than a correctness break.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelProfile {
    /// Stable handle, e.g. `"ork.profiles.gpt-5"` or
    /// `"ork.profiles.qwen-2.5-coder-7b"`. Personas (ADR 0033) and
    /// workflow steps refer to this id.
    pub id: ProfileId,

    /// The canonical `(provider, model)` this profile targets. The
    /// pair is what gets sent on the wire when an agent installed
    /// against this profile is invoked without a more specific
    /// override. Keeping this on the profile makes "for which model
    /// is this profile the recommended one" answerable from a single
    /// lookup (`ModelProfileRegistry::for_model`).
    pub provider_id: String,
    pub model_id: String,

    /// Edit format the model has been validated to drive.
    pub edit_format: EditFormat,

    /// Soft cap on the native-tool catalog exposed to the model
    /// (ADR 0011). When the wired catalog exceeds this number, the
    /// agent loop drops `optional` tools (ADR 0033's `ToolCatalog`
    /// split) before required ones; if `required.len()` still
    /// exceeds the cap the install fails fast.
    pub max_tools_in_catalog: u32,

    /// Absolute token threshold above which ADR 0032's
    /// `ContextCompactor` fires for *this* model. Replaces the
    /// uniform `compaction_trigger_ratio * max_context` heuristic
    /// when a profile is in scope; the ratio remains the fallback
    /// when no profile resolves.
    pub compaction_threshold_tokens: u32,

    /// Default sampling temperature when neither the request nor the
    /// agent config sets one.
    pub default_temperature: f32,

    /// Whether the underlying serving stack accepts grammar /
    /// JSON-Schema-constrained decoding (ADR 0035). When `false`,
    /// the agent loop must not attach a grammar to the request.
    pub supports_grammar_constraint: bool,

    /// Mirrors `ModelCapabilities::supports_tools` for ergonomics;
    /// kept in sync at registry-build time so callers consult one
    /// shape. The wire-level value at `ModelCapabilities` remains
    /// authoritative for negotiation.
    pub supports_native_tool_calls: bool,

    /// Thinking-mode policy for reasoning-tuned models. ADR 0038's
    /// Plan/Execute split honours `PlanningOnly` by enabling thinking
    /// for the Plan phase and disabling it for Execute.
    pub thinking_mode: ThinkingMode,

    /// Optional override: when this profile is attached to an
    /// architect-style persona, prefer this model id for *planner*
    /// invocations even if the operator default points elsewhere.
    /// `None` ⇒ use the resolved provider/model from ADR 0012.
    pub recommended_planner_model_id: Option<String>,

    /// Optional override consumed by ADR 0038's plan-verification
    /// gate: when cross-verification is enabled and the workflow
    /// does not name a verifier, the gate picks a `plan_verifier`
    /// peer whose profile's `model_id` equals this hint (or is
    /// "materially different" from the planner's per ADR 0042's
    /// discovery index, when this hint is `None`). Setting this hint
    /// is the *explicit* way for an operator to pin the verifier
    /// model; ADR 0042 is the implicit fallback.
    pub recommended_plan_verifier_model_id: Option<String>,
}

/// Stable, serialisable handle for a profile. Wire-stable across
/// versions — once published in an agent card, an id never changes
/// shape.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileId(pub String);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    WholeFile,
    SearchReplace,
    UnifiedDiff,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Off,
    PlanningOnly,
    Always,
}
```

`EditFormat` lives in `ork-llm` (not `ork-agents`) because it is a
*model* property — the executor in ADR
[`0029`](0029-workspace-file-editor.md) selects an editor tool *based
on* this enum, but the enum belongs to the model that drives the
choice. ADR [`0033`](0033-coding-agent-personas.md)'s persona-side
`EditFormat` enum (with the extra `ReadOnly` variant) re-exports the
core three from this module and adds `ReadOnly` as a persona-only
variant; that decoupling lets `ork-llm` stay free of any persona
concept.

### Lookup and override chain

```rust
// crates/ork-llm/src/profile.rs (continued)

pub struct ModelProfileRegistry {
    by_id: HashMap<ProfileId, Arc<ModelProfile>>,
    by_model: HashMap<(String, String), ProfileId>, // (provider_id, model_id) -> id
    overrides: HashMap<TenantId, HashMap<ProfileId, Arc<ModelProfile>>>,
}

impl ModelProfileRegistry {
    /// Built-in defaults plus operator config. No tenant overrides
    /// applied; tenant overrides are layered in on `for_tenant`.
    pub fn from_config(cfg: &LlmConfig) -> Result<Self, OrkError>;

    /// Lookup by stable id; honours `tenant`'s override map.
    pub fn get(&self, tenant: &TenantId, id: &ProfileId) -> Option<Arc<ModelProfile>>;

    /// "Which profile applies to a resolved (provider, model)?" Used
    /// by the agent loop when no persona-supplied id is present.
    pub fn for_model(
        &self,
        tenant: &TenantId,
        provider_id: &str,
        model_id: &str,
    ) -> Option<Arc<ModelProfile>>;

    /// All profiles visible to `tenant`, after override application.
    /// Consumed by ADR 0042's discovery index and by the agent-card
    /// publisher.
    pub fn list(&self, tenant: &TenantId) -> Vec<Arc<ModelProfile>>;
}
```

The resolution chain, top wins:

1. **Tenant override** — `[tenants.<id>.model_profiles.<profile_id>]`
   in tenant config, layered in via the same loader ADR
   [`0020`](0020-tenant-security-and-trust.md) uses for other
   tenant-scoped settings. Any subset of fields may be overridden;
   missing fields fall through.
2. **Operator config** — `[llm.profiles]` in the operator config
   ([`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs)),
   loaded into `ModelProfileRegistry` at boot.
3. **Built-in defaults** — a small, hard-coded registry shipped in
   `ork-llm` covering the canonical models the demo and CI exercise.
   The set is intentionally narrow (≤ 8 entries) to discourage drift;
   anything beyond that goes through operator config.

The built-in set ships at minimum:

| Profile id | model_id (canonical) | edit_format | tools cap | compaction tokens | thinking |
| --- | --- | --- | --- | --- | --- |
| `ork.profiles.frontier_planner` | `gpt-5` (operator-overridable) | `unified_diff` | 32 | 120000 | `planning_only` |
| `ork.profiles.frontier_executor` | `claude-sonnet-4-6` | `unified_diff` | 24 | 120000 | `off` |
| `ork.profiles.frontier_verifier` | `claude-haiku-4-5` | `unified_diff` (read-only personas ignore) | 16 | 80000 | `off` |
| `ork.profiles.local_coder_small` | `qwen-2.5-coder-7b` | `whole_file` | 6 | 12000 | `off` |
| `ork.profiles.local_coder_medium` | `qwen-2.5-coder-32b` | `search_replace` | 10 | 24000 | `off` |
| `ork.profiles.local_general` | `llama-3-8b-instruct` | `whole_file` | 4 | 6000 | `off` |

Concrete numbers in the table are guidance, not a wire contract — the
implementing session may tune them based on dogfooding data captured
under ADR [`0022`](0022-observability.md) and update the
`ModelProfile` defaults inline. The *shape* (six fields above plus the
verifier-hint fields) is the contract.

### `LocalAgent` integration

`LocalAgent::run` (currently calling out to `LlmRouter` for
capabilities-only resolution) gains a `Arc<ModelProfileRegistry>`
dependency wired through the same constructor as `LlmRouter`. At each
loop iteration, after resolving the `(provider, model)` pair, it
materialises an *effective* profile in this order:

1. If `AgentConfig::persona` is `Some` and ADR [`0033`]'s
   `ModelProfileRef` resolves to a profile id, use that.
2. Else `ModelProfileRegistry::for_model(tenant, provider, model)`.
3. Else a `ModelProfile::neutral_default()` (no overrides, no
   recommendations, edit_format = `WholeFile`,
   `compaction_threshold_tokens = caps.max_context * 0.8` rounded
   down, temperature `0.7`, every "supports_*" flag false).

The effective profile then drives:

- **(a) Edit-format-aware tool selection (ADR 0029).** `LocalAgent`
  resolves the editor tool by `profile.edit_format`:
  `WholeFile → write_file`, `SearchReplace → update_file`,
  `UnifiedDiff → apply_patch`. Personas marked `ReadOnly` (ADR
  [`0033`]) bypass this step and never see an editor tool.
- **(b) Tool-catalog trim.** Before each `chat_stream`, if
  `wired_catalog.len() > profile.max_tools_in_catalog`, drop
  `optional` tools first (ADR [`0033`]'s `ToolCatalog` split). If the
  trimmed catalog still exceeds the cap, fail with
  `OrkError::Validation("tool_catalog_exceeds_profile_cap")`.
- **(c) Compaction threshold.** ADR
  [`0032`](0032-agent-memory-and-context-compaction.md)'s loop fires
  compaction when `estimator.estimate_request(...) >
  profile.compaction_threshold_tokens` instead of
  `caps.max_context * trigger_ratio`. The ratio remains the fallback
  for the neutral default.
- **(d) Grammar constraint (ADR 0035).** ADR [`0035`]'s grammar
  attachment is gated on `profile.supports_grammar_constraint`; when
  `false`, the agent loop strips any grammar field from the request
  before sending and emits a `tracing::debug!(target = "ork.profile",
  grammar_skipped = true)` event.
- **(e) Plan vs Execute thinking-mode default (ADR 0038).** When ADR
  [`0038`] is enabled and the current step is `PersonaPhase::Plan` or
  `PersonaPhase::Verify`, the loop sets the provider's thinking flag
  per `profile.thinking_mode == Always | PlanningOnly`. For
  `PersonaPhase::Edit` and later, only `Always` keeps thinking on;
  `PlanningOnly` and `Off` disable it.
- **(f) Default plan-verifier pick (ADR 0038).** When ADR [`0038`]'s
  cross-verification gate fires and the workflow does not name a
  verifier endpoint, the gate consults the *planner's* profile and
  picks a verifier peer whose own profile satisfies:
  - if `planner_profile.recommended_plan_verifier_model_id ==
    Some(m)`, prefer a peer with `profile.model_id == m`;
  - else, defer to ADR [`0042`]'s discovery, which indexes profiles
    and ranks "materially different" peers by Levenshtein distance on
    `provider_id` plus a same-vendor penalty (so `gpt-5 →
    claude-sonnet-4-6` ranks above `gpt-5 → gpt-5-mini`).

### Agent card extension (ADR 0005 surface)

A new ork extension URI is reserved:

```
https://ork.dev/a2a/extensions/model-profile
```

Card payload (added to the `extensions` array on the agent card —
same surface ADR [`0005`](0005-agent-card-and-devportal-discovery.md)
already uses for `transport-hint` and `tenant-required`, and ADR
[`0033`](0033-coding-agent-personas.md)'s `coding-persona`
extension):

```json
{
  "uri": "https://ork.dev/a2a/extensions/model-profile",
  "params": {
    "profile_id": "ork.profiles.frontier_planner",
    "model_id": "gpt-5",
    "edit_format": "unified_diff",
    "max_tools_in_catalog": 32,
    "supports_grammar_constraint": false,
    "supports_native_tool_calls": true,
    "thinking_mode": "planning_only",
    "recommended_planner_model_id": null,
    "recommended_plan_verifier_model_id": "claude-haiku-4-5"
  }
}
```

`compaction_threshold_tokens` and `default_temperature` are
deliberately **not** advertised — they are operational tuning that
varies per deployment and would mislead a remote consumer indexing on
them. Everything in the published payload is something a *peer* would
need to know to decide whether to delegate to or cross-verify against
this agent (ADR [`0042`]).

The shape is forward-compatible: ADR [`0042`] may add fields to
`params`; existing consumers ignore unknown keys per the A2A
extension spec. New `EditFormat` and `ThinkingMode` variants are
additive enums.

### Multi-tenant override surface

Operator config gains a `[llm.profiles]` block parallel to
`[llm.providers]`:

```toml
[[llm.profiles]]
id = "ork.profiles.local_coder_small"
provider_id = "gpustack"
model_id = "qwen-2.5-coder-7b"
edit_format = "whole_file"
max_tools_in_catalog = 6
compaction_threshold_tokens = 12000
default_temperature = 0.2
supports_grammar_constraint = true
supports_native_tool_calls = false
thinking_mode = "off"
```

Tenant config layers in:

```toml
# tenants/<tid>.toml
[[model_profiles]]
id = "ork.profiles.local_coder_small"
# Override only the fields you want; missing fields fall through to
# operator config and then to the built-in default.
default_temperature = 0.0
recommended_plan_verifier_model_id = "qwen-2.5-coder-32b"
```

The loader at `ModelProfileRegistry::from_config` rejects an entry
whose `id` collides with a built-in *and* whose `provider_id` or
`model_id` differs (preventing accidental override-by-typo); same-id
same-target overrides are accepted and merge field-wise.

### Out of scope

- **Per-tenant model overrides at request time.** ADR
  [`0012`](0012-multi-llm-providers.md) already covers
  `(provider, model)` resolution per request via
  `ResolveContext`; this ADR layers profile lookup on top of the
  *result* of that resolution and does not change it.
- **Cost / latency telemetry on the profile.** ADR
  [`0022`](0022-observability.md) owns `ork_llm_*` metrics and the
  per-tenant cost rollup. Adding cost or p95-latency fields to
  `ModelProfile` would conflate "tuning" and "measurement"; cost
  table lookups stay where ADR
  [`0032`](0032-agent-memory-and-context-compaction.md) put them
  (`LlmConfig::cost_table`).
- **Persona / role descriptors.** Owned by ADR
  [`0033`](0033-coding-agent-personas.md). This ADR provides what
  ADR [`0033`]'s `ModelProfileRef` resolves to.
- **Discovery indexing.** Owned by ADR [`0042`]. This ADR provides
  the wire payload (`model-profile` extension) that ADR [`0042`]
  ingests.
- **Grammar / structured-decoding payload format.** Owned by ADR
  [`0035`]. This ADR provides only the gate
  (`supports_grammar_constraint`).
- **Cross-verification protocol semantics.** Owned by ADR
  [`0038`]. This ADR provides only the verifier-model hint and the
  Plan/Execute thinking-mode policy that the protocol consumes.

## Acceptance criteria

- [ ] Type `ModelProfile` defined at
      [`crates/ork-llm/src/profile.rs`](../../crates/ork-llm/src/) with
      every field in `Decision` (id, provider_id, model_id,
      edit_format, max_tools_in_catalog,
      compaction_threshold_tokens, default_temperature,
      supports_grammar_constraint, supports_native_tool_calls,
      thinking_mode, recommended_planner_model_id,
      recommended_plan_verifier_model_id); derives
      `Clone + Debug + Serialize + Deserialize`.
- [ ] Enums `EditFormat` (`whole_file | search_replace |
      unified_diff`) and `ThinkingMode` (`off | planning_only |
      always`) defined in the same module with
      `#[serde(rename_all = "snake_case")]`.
- [ ] Newtype `ProfileId(String)` defined in the same module with
      `Eq + Hash + Serialize + Deserialize`.
- [ ] `ModelProfile::neutral_default(caps: &ModelCapabilities) ->
      ModelProfile` constructor returns a profile with
      `edit_format = WholeFile`, `compaction_threshold_tokens =
      (caps.max_context as f32 * 0.8) as u32` (or `0` when
      `caps.max_context == 0`), `default_temperature = 0.7`, both
      `supports_*` flags `false`, `thinking_mode = Off`, no
      recommendation hints.
- [ ] Type `ModelProfileRegistry` with `from_config`, `get`,
      `for_model`, and `list` methods as in `Decision`; lives in
      [`crates/ork-llm/src/profile.rs`](../../crates/ork-llm/src/).
- [ ] Built-in registry contains at least the six profiles in the
      table under `Decision`, registered by
      `ModelProfileRegistry::with_builtins`.
- [ ] `[llm.profiles]` parsed by
      [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs)
      into a `Vec<ModelProfileEntry>` with the field shape shown in
      `Decision`; missing optional fields fall through to built-in
      defaults at registry build.
- [ ] Tenant override loader (under ADR
      [`0020`](0020-tenant-security-and-trust.md)'s tenant config
      pipeline) layers `[[model_profiles]]` entries into the
      registry; field-level merge (override only declared fields)
      verified by
      `crates/ork-llm/tests/profile_overrides.rs::tenant_override_merges_field_wise`.
- [ ] `from_config` returns
      `Err(OrkError::Configuration("profile_id_collision_with_different_target"))`
      when an operator entry shares a built-in id but declares a
      different `provider_id` or `model_id`.
- [ ] `LocalAgent::new` (in
      [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs))
      accepts an `Arc<ModelProfileRegistry>`; the existing
      constructor signature stays compilable via a
      `LocalAgentBuilder` shim that defaults to a registry with only
      built-ins.
- [ ] **(a)** Edit-format-aware tool selection: integration test
      `crates/ork-agents/tests/profile_edit_format.rs::picks_apply_patch_for_unified_diff`
      installs a profile with `edit_format = UnifiedDiff` and
      asserts the agent loop calls `apply_patch` (not `write_file`)
      on a stub editor.
- [ ] **(b)** Catalog trim: integration test
      `crates/ork-agents/tests/profile_catalog_trim.rs::drops_optional_tools_first`
      wires a 12-tool catalog (4 required + 8 optional) against a
      profile with `max_tools_in_catalog = 6` and asserts the request
      to the stub `LlmProvider` carries exactly the 4 required +
      first 2 optional tools.
- [ ] **(b cont.)** Same test asserts that when `required.len() = 8`
      against a cap of `6`, `LocalAgent::run` returns
      `OrkError::Validation("tool_catalog_exceeds_profile_cap")`
      *before* the first `chat_stream` call.
- [ ] **(c)** Compaction threshold: integration test
      `crates/ork-agents/tests/profile_compaction.rs::fires_at_profile_threshold`
      installs a profile with `compaction_threshold_tokens = 1000`
      and a stub `TokenEstimator` returning `1500`; asserts
      `ContextCompactor::compact` is called exactly once and the
      resulting `tracing::info!(target = "ork.cost", ...)` event
      carries `compaction_fired = true` and the profile id.
- [ ] **(d)** Grammar gate: integration test
      `crates/ork-agents/tests/profile_grammar.rs::strips_grammar_when_unsupported`
      builds a `ChatRequest` with a grammar attachment, runs against
      a profile with `supports_grammar_constraint = false`, and
      asserts the request reaching the stub provider has no grammar
      field; a `tracing::debug!(target = "ork.profile",
      grammar_skipped = true)` event is emitted.
- [ ] **(e)** Thinking-mode policy: integration test
      `crates/ork-agents/tests/profile_thinking_mode.rs::planning_only_disables_in_edit_phase`
      installs a profile with `thinking_mode = PlanningOnly` and
      asserts the request reaching the provider has thinking enabled
      when `step.phase == Plan` and disabled when `step.phase ==
      Edit`.
- [ ] **(f)** Verifier-model hint: unit test
      `crates/ork-llm/tests/profile_verifier_hint.rs::recommended_plan_verifier_round_trips`
      asserts `ModelProfile::recommended_plan_verifier_model_id`
      survives a registry round-trip and is exposed on
      `ModelProfileRegistry::list` output.
- [ ] Card extension serde struct `ModelProfileCardExtension` defined
      at `crates/ork-a2a/src/extensions/model_profile.rs` with the
      JSON shape in `Decision`; `crates/ork-a2a/tests/extensions_model_profile.rs::roundtrip`
      asserts the example payload deserialises and re-serialises byte-stable.
- [ ] When a `LocalAgent` is built with a non-default profile,
      `Agent::card()` includes one extension entry with
      `uri = "https://ork.dev/a2a/extensions/model-profile"` and
      params populated from the *redacted* projection (no
      `compaction_threshold_tokens`, no `default_temperature`) —
      verified by
      `crates/ork-agents/tests/profile_card.rs::extension_present_and_redacted`.
- [ ] `ModelProfile::redacted_for_card` helper documented as the
      single source of truth for what is published; the test above
      goes through it.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0034`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- The same `LocalAgent` runs sensibly on a frontier hosted model and
  a 7B local model — the agent loop's six decision points consult one
  declarative shape per model instead of being silently mis-tuned by
  the operator default.
- Operators can roll out a new local model by writing one
  `[[llm.profiles]]` entry; tenants can override per-deployment
  without code changes, satisfying ADR
  [`0020`](0020-tenant-security-and-trust.md)'s isolation
  requirement.
- ADR [`0033`](0033-coding-agent-personas.md)'s `ModelProfileRef`
  becomes resolvable; until this ADR lands the persona installer
  treats the ref as advisory.
- ADR [`0038`]'s plan-verification gate gets an explicit hint
  (`recommended_plan_verifier_model_id`) for picking a non-echoing
  verifier — and a deterministic fallback (ADR [`0042`]'s discovery
  index, which now has the `model-profile` extension to rank on).
- ADR [`0035`]'s grammar-constrained decoding can land safely; the
  flag tells the loop whether it is allowed to attach a grammar at
  all.
- The card extension is a small, additive payload that lets a
  *peer* mesh decide whether to delegate to or cross-verify against
  this agent without an out-of-band catalog.

### Negative / costs

- Two parallel surfaces — `ModelCapabilities` (wire) and
  `ModelProfile` (behaviour) — increase the configuration surface.
  Operators have to learn which one they want to touch. We accept
  this because collapsing them muddles correctness with tuning, but
  the docs in `crates/ork-llm/src/profile.rs` must explain the
  distinction (the acceptance criterion above on
  `redacted_for_card` reinforces this).
- The built-in default registry is opinionated. A model not in the
  built-in set falls back to `ModelProfile::neutral_default`, which
  is conservative but pessimistic — operators may see degraded
  behaviour until they add the profile. Mitigated by emitting a
  one-time `tracing::warn!(target = "ork.profile",
  using_neutral_default = true)` per `(provider, model)` pair the
  first time it is resolved.
- `compaction_threshold_tokens` is a magic number per model. It will
  rot as providers extend context windows; the table in `Decision`
  is guidance, not a contract. We accept this — the alternative
  ("derive from a percentage of `max_context`") was the previous
  uniform behaviour and the ADR's whole point is that one ratio does
  not fit all.
- The override chain has three layers (built-in → operator →
  tenant). Debugging "why did this profile resolve this way?" needs
  introspection. The acceptance criterion on `for_model` test
  coverage exercises the full chain; we additionally require a
  `LlmConfig::dump_resolved_profile(tenant, model_id)` helper as
  part of implementation (not in the acceptance list because it is a
  diagnostic, not a feature).
- Card-extension publication leaks a model id into the discovery
  surface. Operators with sensitive model choices may want to
  suppress this; the redacted projection drops tuning fields, but
  `model_id` itself is visible. Mitigation: `LlmConfig` gains a
  per-profile `publish_on_card: bool` (default `true`); when `false`
  the extension is omitted. This is **not** in the acceptance
  criteria for v1; revisit in `Open questions` if a tenant requests
  it. (The redacted-by-default *content* of the card is still safer
  than nothing.)
- ADR [`0033`]'s `EditFormat` re-export creates a small one-way
  coupling: persona code now imports a type from `ork-llm`. Since
  `ork-agents` already depends on `ork-llm` in the workspace's
  `Cargo.toml`, no new dep is introduced.
- The verifier-model hint encodes vendor knowledge into config (e.g.
  "claude is materially different from gpt"). When a new vendor
  arrives, ADR [`0042`]'s ranking heuristic has to learn it. We
  accept this; the alternative — having ork compute model
  similarity at runtime — is out of scope and likely not worth the
  complexity.
- A profile's `provider_id` and `model_id` duplicate what ADR
  [`0012`](0012-multi-llm-providers.md) already tracks per provider
  config. This duplication is intentional: it makes
  `ModelProfileRegistry::for_model` an `O(1)` lookup without
  reaching into `LlmRouter`. We accept the small risk of drift; the
  loader rejects collision-with-different-target entries to bound
  it.

### Neutral / follow-ups

- ADR [`0042`]'s discovery service indexes the `model-profile`
  extension and is expected to add filterable fields to `params`;
  the wire contract is forward-compatible for additive keys.
- ADR [`0035`] consumes `supports_grammar_constraint` and is the
  natural place to add a richer "grammar dialect" enum (lark, gbnf,
  json-schema). When that lands, this ADR's flag stays as a coarse
  on/off gate.
- ADR [`0038`] consumes the verifier-model hint and the Plan/Execute
  thinking policy. Its protocol design is independent of the
  *values* on the profile; it only depends on the *shape*.
- ADR [`0045`] (multi-agent teams) routes personas to profiles via
  ADR [`0033`]'s `ModelProfileRef`. No further wire change is
  expected from 0045 to this ADR.
- A future ADR may add `cost_per_million_tokens` and
  `p95_latency_ms` fields *adjacent to* `ModelProfile` (not on it)
  driven by ADR [`0022`](0022-observability.md)'s telemetry — a
  measured-profile complement to this configured-profile.
- An MCP-style tool to expose the profile registry to operators
  (`list_model_profiles`, `dump_resolved_profile`) is a small
  follow-up; tracked in `Open questions`.

## Alternatives considered

- **Extend `ModelCapabilities` instead of adding a parallel
  `ModelProfile`.** Simpler in one sense (one struct). Rejected:
  `ModelCapabilities` is consulted as a *correctness* gate
  (`supports_tools` decides whether tool calls are even sent); a
  wrong `compaction_threshold_tokens` is a *performance* regression.
  Loading both into one struct would obscure the contract — every
  field would need a "is this safety or tuning?" annotation. Two
  parallel surfaces with one redundant field
  (`supports_native_tool_calls` mirrors `supports_tools`) is the
  smaller cost.
- **Profiles keyed by `(provider, model)` only — no stable
  `ProfileId`.** Rejected: ADR [`0033`]'s persona references a
  profile by name (`ork.profiles.solo_coder`) so the same persona can
  retarget different `(provider, model)` pairs across deployments
  without rewriting the persona. A stable id also lets ADR
  [`0042`]'s discovery index publish a `profile_id` that survives
  vendor migrations.
- **All defaults in a YAML file shipped under `config/`.** Rejected:
  the built-in set is intentionally tiny and exercised by tests; a
  YAML drift would be silently wrong. Operators add to it via the
  documented `[[llm.profiles]]` block.
- **Profile lookup at `LlmRouter` level instead of `LocalAgent`.**
  Symmetric with how `ModelCapabilities` resolution works today.
  Rejected: the *consumers* of the profile are agent-loop concerns
  (compaction, tool catalog trim, edit-format selection); putting
  the lookup in `LlmRouter` would require the router to surface a
  rich profile object on every call, polluting the wire-narrow
  `LlmProvider` trait. The agent loop is the right consumer; the
  router stays compatibility-focused.
- **Tenant overrides via dynamic admin API instead of config.**
  Rejected for v1: ADR [`0020`](0020-tenant-security-and-trust.md)'s
  config layering is the only currently-supported tenant override
  surface; introducing a runtime admin API for profiles would
  duplicate work that belongs to a hypothetical "tenant-config CRUD"
  ADR. Operators who need runtime updates today restart with the new
  config — same as for every other tenant-scoped setting.
- **Use the persona's `model_profile_ref` as the *only* lookup
  pathway; drop the by-`(provider, model)` lookup.** Rejected: not
  every agent has a persona attached. The neutral default and the
  by-model lookup are needed for legacy `LocalAgent` instances and
  for ADR [`0042`]'s discovery, which receives a `(provider, model)`
  on the wire and asks "what is the canonical profile for this
  pair?".
- **Bake the verifier-model hint into ADR [`0038`]'s protocol
  payload (e.g. on every `Plan` message).** Rejected: that would
  couple per-message wire shape to operator-tunable hints. Putting
  the hint on the profile keeps the wire stable and the hint
  resolved at the calling end.
- **Compute "materially different" at this ADR's level (e.g. via a
  vendor-similarity table).** Rejected: ADR [`0042`]'s discovery is
  the right place — it has the full mesh visibility and a ranking
  surface. Encoding similarity here would duplicate an index ADR
  [`0042`] is going to build anyway, and would couple this ADR to a
  vendor list that grows over time.
- **Single `ThinkingMode::Auto` that the runtime infers from
  `(provider, model, phase)`.** Rejected: inference at request time
  is brittle (a small model behind a "thinking-capable" route would
  be silently flagged). Explicit per-profile policy is the simpler
  contract; ADR [`0042`]'s discovery can suggest sensible profile
  defaults for new models, but the profile is what the loop reads.

## Affected ork modules

- New: [`crates/ork-llm/src/profile.rs`](../../crates/ork-llm/src/) —
  `ModelProfile`, `ProfileId`, `EditFormat`, `ThinkingMode`,
  `ModelProfileRegistry`, `ModelProfileEntry` (config-shaped),
  `redacted_for_card`, `neutral_default`.
- [`crates/ork-llm/src/lib.rs`](../../crates/ork-llm/src/lib.rs) —
  re-export the profile surface.
- [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs) —
  no behavioural change; new constructor parameter
  `Arc<ModelProfileRegistry>` plumbed through for
  `LocalAgent::new`.
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) —
  `[[llm.profiles]]` parsed into `Vec<ModelProfileEntry>`.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) —
  effective-profile resolution at start of `run`; six decision
  points (a)–(f) above; profile id forwarded onto the
  `ork.cost` event from ADR
  [`0032`](0032-agent-memory-and-context-compaction.md) for
  observability.
- New: [`crates/ork-a2a/src/extensions/model_profile.rs`](../../crates/ork-a2a/) —
  serde struct + URI constant for the
  `https://ork.dev/a2a/extensions/model-profile` extension; matching
  unit tests at
  `crates/ork-a2a/tests/extensions_model_profile.rs`.
- ADR
  [`0033`](0033-coding-agent-personas.md)'s `EditFormat` re-exports
  the three core variants from `ork-llm` and adds the persona-only
  `ReadOnly` variant locally; a tracking note lands in ADR
  [`0033`]'s `Reviewer findings` when this ADR is implemented.
- [`docs/adrs/README.md`](README.md) — ADR index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Aider | `MODEL_SETTINGS` table per model id (edit format, weak/strong, max tokens, reasoning settings) | `ModelProfile` with `edit_format`, `compaction_threshold_tokens`, `thinking_mode` |
| LiteLLM | Per-model `model_info` dict driving routing and feature flags | `ModelProfile` + override chain |
| Continue.dev | `models[].capabilities` and `defaultCompletionOptions` per provider | `ModelProfile` + `default_temperature` |
| OpenRouter | Per-model `top_provider.context_length`, `pricing`, `supports_tool_calls` | `ModelProfile` (subset, minus pricing — that's ADR 0022's lane) |
| Anthropic / OpenAI | "thinking" / "reasoning effort" knobs as request-time params | `ThinkingMode` on the profile, applied at the loop's Plan/Execute boundary |
| GPUStack / vLLM | Per-deployment `max_tokens`, grammar / guided-decoding flags | `compaction_threshold_tokens`, `supports_grammar_constraint` |

## Open questions

- **Per-profile card-publication suppression.** Operators with
  sensitive model choices may want to omit the `model-profile`
  extension from the agent card. Stance: add a `publish_on_card:
  bool` field if a tenant requests it; the redacted projection is
  the v1 mitigation. Out of scope for the acceptance criteria.
- **Profile versioning.** Same problem ADR [`0033`] flagged for
  personas. Stance: id-suffix when needed (`ork.profiles.gpt-5-v2`);
  unknown-key tolerance on the wire keeps the path open.
- **Where do reasoning-effort-as-integer models fit?** Some OpenAI
  models accept a 0–10 reasoning effort. `ThinkingMode` is a
  three-valued enum; richer semantics belong in ADR [`0038`]'s
  protocol or in a follow-up `ReasoningEffort` field. Defer.
- **Profile inference for unknown models.** Should the registry
  attempt to *guess* a profile for an unrecognised `(provider,
  model)` (e.g. heuristic on the model name)? Stance: no — emit the
  warning, use `neutral_default`, document the pattern in the
  operator docs. Inference is a foot-gun in this layer.
- **MCP exposure of the registry.** `list_model_profiles` /
  `dump_resolved_profile` as native tools (not MCP per
  [`AGENTS.md`](../../AGENTS.md) §3) for operator agents. Defer
  until an operator agent persona exists.
- **Grammar dialect.** ADR [`0035`] will need a richer enum
  (`lark | gbnf | json_schema`) once concrete servers are wired.
  This ADR's `supports_grammar_constraint` stays as a boolean gate;
  ADR [`0035`] adds the dialect field on its own struct.

## References

- A2A spec — extensions: <https://github.com/google/a2a>
- Aider model settings:
  <https://aider.chat/docs/llms/editing-format.html>
- LiteLLM model info:
  <https://docs.litellm.ai/docs/completion/model_alias>
- OpenRouter API — model metadata:
  <https://openrouter.ai/docs/api-reference/list-available-models>
- vLLM guided decoding:
  <https://docs.vllm.ai/en/latest/usage/structured_outputs.html>
- GPUStack:
  <https://docs.gpustack.ai/>
- Related ADRs:
  [`0005`](0005-agent-card-and-devportal-discovery.md),
  [`0011`](0011-native-llm-tool-calling.md),
  [`0012`](0012-multi-llm-providers.md),
  [`0020`](0020-tenant-security-and-trust.md),
  [`0022`](0022-observability.md),
  [`0029`](0029-workspace-file-editor.md),
  [`0032`](0032-agent-memory-and-context-compaction.md),
  [`0033`](0033-coding-agent-personas.md),
  0035 (forthcoming),
  0038 (forthcoming),
  0042 (forthcoming).
