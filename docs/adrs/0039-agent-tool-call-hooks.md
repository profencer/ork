# 0039 — Agent tool-call hooks

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0011, 0021, 0022, 0024, 0025, 0027, 0033, 0038, 0042, 0043, 0045
- **Supersedes:** —

## Context

The `LocalAgent` tool-call loop in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
takes the `ToolCall`s the LLM emits, hands each one to a
`ToolExecutor` (currently the `CompositeToolExecutor` at
[`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)),
and feeds the result back as a `ChatMessage::tool` for the next
iteration. The only safety net between "LLM emitted a call" and
"call ran" today is `is_fatal_tool_error` (post-hoc error
classification) and whatever validation the executor itself does.
That is not enough for three classes of guardrail an operator
routinely wants:

1. **"Don't run that command."** Block `run_command` invocations
   whose argv matches a tenant-defined pattern (e.g. `rm -rf`,
   `curl … | sh`) before the executor sees them. This is a
   guardrail, not authorization — RBAC (ADR
   [`0021`](0021-rbac-scopes.md)) decides whether a *principal*
   may invoke `run_command` at all; hooks decide whether *this
   specific argv* is allowed for that tool the principal already
   has access to.
2. **"Redact this from the result."** Strip API tokens, customer
   PII, or path prefixes out of a tool result *before* it lands
   on `history` (and therefore before the LLM ingests it on the
   next turn). Post-hoc verification (ADR
   [`0025`](0025-typed-output-validation-and-verifier-agent.md))
   runs on typed step outputs — too late and too coarse for
   per-call redaction.
3. **"Gate this on a peer review."** Block the `propose_plan`
   tool from completing until ADR
   [`0038`](0038-plan-mode-and-cross-verification.md)'s
   plan-cross-verification gate has run; on `request_changes`
   return the verifier findings to the planner *as the tool
   result* so the LLM can revise. ADR
   [`0038`](0038-plan-mode-and-cross-verification.md) defines
   the protocol; this ADR is its enforcement point in the loop.

Frontier coding harnesses converged on a small, sharp surface
for this: Claude Code's `PreToolUse` / `PostToolUse` hooks,
opencode's pre/post-tool hooks, Aider's `--pre-commit` shell
hook. They are all variants of the same idea: a synchronous,
allow-list-shaped callback fired before and after every tool
call, returning **allow / deny / modify**. Hooks are strictly
*less powerful* than RBAC — they cannot grant access — but
vastly *cheaper to configure* than a scope policy because they
live in user-authored config and need no central authority to
edit. In ork that gap matters: ADR
[`0021`](0021-rbac-scopes.md) is the platform-level
authorization plane, but nine times out of ten an operator just
wants "don't ever let an agent run `git push --force`" and
RBAC is a sledgehammer for that.

ork has every prerequisite to land hooks:

- ADR [`0011`](0011-native-llm-tool-calling.md) already has the
  single seam (`execute_tool_call` in `local.rs`) where every
  tool call passes through; one function, no per-tool duplication.
- ADR [`0033`](0033-coding-agent-personas.md)'s
  `CodingPersona` already carries per-persona configuration that
  can name hook bundles.
- ADR [`0022`](0022-observability.md) already defines a
  per-task event log and audit stream into which hook decisions
  belong.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md) names a
  `propose_plan` tool whose Plan→Execute transition has to be
  gated *somewhere*; the hook surface is the natural place — it
  fires per tool-call and can short-circuit the call with a
  reason returned to the LLM.
- ADR [`0024`](0024-wasm-plugin-system.md) already chose WASM
  as ork's extension path; "user-authored arbitrary code" hooks
  are deliberately **out of scope** for v1 and live in the WASM
  plugin runtime when needed.

The closest existing surfaces are deliberately *not* this:

- **RBAC (ADR [`0021`](0021-rbac-scopes.md))** authorizes
  *principals* to invoke *tools* over *resources*. RBAC denies
  first — a tool call that fails RBAC never reaches a hook.
  Hooks are user-authored guardrails on calls RBAC has already
  permitted.
- **Verifier port (ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md))**
  fires on a step's *typed output* after the producing step
  finishes. Hooks fire on *individual tool-calls* inside that
  step, before and after each.
- **Plan cross-verification (ADR
  [`0038`](0038-plan-mode-and-cross-verification.md))** *is* the
  protocol that gates Plan→Execute. Hooks *trigger* it via the
  builtin `require_a2a_plan_verification` kind defined here;
  without a hook configured (or a workflow step opting in), the
  gate does not fire.
- **WASM plugins (ADR [`0024`](0024-wasm-plugin-system.md))**
  are the path for arbitrary user-authored hook code. v1 hooks
  are a closed set of *builtins* selected by name with config;
  unbounded scripting is deferred.

A team-orchestrator world (ADR [`0045`], forthcoming) needs hook
*composition*: tenant-level hooks an admin sets once; team-level
hooks (ADR [`0043`], forthcoming) a tech lead can add for their
team; persona-level hooks an agent author bundles with their
persona. These three layers compose in a defined order with
defined precedence so adding a team-level hook never weakens a
tenant-level one. ADR [`0043`] is not landed yet, so the team
scope is a typed but no-op placeholder until it does.

This ADR specifies the hook trait, the configuration scopes
and composition rules, the closed builtin set including the
`require_a2a_plan_verification` plan-mode gate, the integration
point in `LocalAgent`, and the observability contract. It does
**not** implement any tool-side change beyond hook plumbing,
and explicitly defers user-authored / WASM hooks.

## Decision

ork **introduces** an `AgentHook` trait with two phases —
`PreToolCall(name, args)` and `PostToolCall(name, args, result)`
— each returning a `HookOutcome` of `Allow`, `Deny(reason)`, or
`Modify(new_args | new_result)`. Hooks are configured at three
scopes — **tenant**, **team**, **persona** — and composed in
that order with `Deny`-wins-first semantics. v1 ships a closed
set of **builtin** hook kinds; user-authored arbitrary code is
deferred to ADR [`0024`](0024-wasm-plugin-system.md). The hook
chain runs inside `LocalAgent::execute_tool_call`'s seam and a
`Deny` short-circuits the call: the tool never runs, and the
LLM receives the `reason` string as the tool result so it can
adapt on the next iteration. Every hook decision lands on ADR
[`0022`](0022-observability.md)'s per-task event log.

### Trait

```rust
// crates/ork-agents/src/hooks.rs

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[async_trait]
pub trait AgentHook: Send + Sync {
    /// Stable identifier. Persisted in audit events; must match
    /// the kind name in config (e.g. "deny_command_pattern").
    fn kind(&self) -> &'static str;

    /// Pre-tool-call phase. Runs **before** the tool executor
    /// is invoked. May deny (short-circuit), modify the
    /// arguments, or allow.
    async fn pre_tool_call(
        &self,
        ctx: &HookContext,
        call: &PreToolCall,
    ) -> Result<HookOutcome<Value>, OrkError> {
        let _ = (ctx, call);
        Ok(HookOutcome::Allow)
    }

    /// Post-tool-call phase. Runs **after** the tool executor
    /// returns. May deny (turn a successful call into a denied
    /// tool result), modify the result before it lands on
    /// `history`, or allow.
    async fn post_tool_call(
        &self,
        ctx: &HookContext,
        call: &PostToolCall,
    ) -> Result<HookOutcome<Value>, OrkError> {
        let _ = (ctx, call);
        Ok(HookOutcome::Allow)
    }
}

#[derive(Clone, Debug)]
pub struct PreToolCall<'a> {
    pub tool_name: &'a str,
    pub arguments: &'a Value,
    pub call_id: &'a str,
}

#[derive(Clone, Debug)]
pub struct PostToolCall<'a> {
    pub tool_name: &'a str,
    pub arguments: &'a Value,
    pub call_id: &'a str,
    pub result: &'a Value,
}

/// Result of a hook phase. The type parameter is `Value` for
/// arguments in pre and `Value` for the result in post; the
/// shape is symmetric.
#[derive(Clone, Debug)]
pub enum HookOutcome<T> {
    /// Continue the chain. The next hook (or the executor)
    /// sees the unchanged inputs.
    Allow,
    /// Short-circuit. The reason is returned to the LLM as the
    /// tool result and the call is recorded as denied.
    Deny { reason: String, finding_id: Option<String> },
    /// Continue the chain with the modified value. In
    /// `pre_tool_call` this rewrites `arguments`; in
    /// `post_tool_call` it rewrites `result`.
    Modify(T),
}

#[derive(Clone)]
pub struct HookContext {
    pub tenant_id: TenantId,
    pub team_id: Option<TeamId>,           // None until ADR 0043 lands
    pub persona_id: Option<PersonaId>,     // ADR 0033
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub context_id: ContextId,
    pub principal: Principal,              // ADR 0021
    pub events: Arc<dyn EventSink>,        // ADR 0022
    pub artifacts: Arc<dyn ArtifactStore>, // ADR 0016 (used by log_to_artifact)
    pub a2a_dispatcher: Arc<dyn PeerDispatcher>, // ADR 0006/0007 (used by require_a2a_plan_verification)
}
```

The trait is intentionally narrow: two methods, one outcome
shape, no per-tool specialisation. A hook that wants to act on
exactly one tool name matches by name in its own body.

### Configuration scopes and composition

Hooks are configured in three scopes, each scoped to a registry
entry that the agent loop resolves once at the start of a step:

```rust
// crates/ork-agents/src/hooks.rs (continued)

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    Tenant,
    Team,
    Persona,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HookConfig {
    /// Stable kind name; resolves to a builtin registered in
    /// `BuiltinHookRegistry::for_kind`.
    pub kind: String,
    /// Tool-name match. None ⇒ apply to every tool call.
    /// Patterns use the same glob shape as ADR 0021 scopes
    /// (`agent_call`, `git_*`, `*`).
    pub applies_to: Option<String>,
    /// Phase mask: pre, post, or both. Default both.
    #[serde(default = "HookConfig::default_phases")]
    pub phases: HookPhaseMask,
    /// Builtin-specific config; opaque to the chain.
    #[serde(default)]
    pub config: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookPhaseMask {
    pub pre: bool,
    pub post: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HookChain {
    pub tenant: Vec<Arc<dyn AgentHook>>,
    pub team: Vec<Arc<dyn AgentHook>>,    // empty until ADR 0043
    pub persona: Vec<Arc<dyn AgentHook>>,
}
```

**Composition order is fixed:** tenant → team → persona. The
chain runs in that order for `pre_tool_call`; the **reverse**
order for `post_tool_call` (so a tenant policy that wraps the
result is the outermost layer in both directions, mirroring
middleware conventions). Within a scope, hooks run in the order
they are configured.

**Deny wins, first.** The first `HookOutcome::Deny` from any
scope short-circuits the chain. A subsequent hook cannot
override an earlier deny. Rationale: a tenant policy must not
be loosenable by a team or persona below it; the only way to
make a guardrail more permissive is to remove it from the
higher scope.

**`Modify` accumulates.** Each hook sees the cumulative
modified value from earlier hooks in the chain. If two hooks
modify the arguments, the second sees the first's output.

**Team scope is currently no-op.** Until ADR [`0043`] (team
identity) lands, `HookContext::team_id` is `None`,
`HookChain::team` is empty, and team-scope config is silently
dropped at load time with a `tracing::info!` audit. This is a
typed placeholder, not a deferred design decision: when 0043
lands, the only change is `HookContext::team_id` becoming
`Some(_)` and `HookChain::team` becoming populated.

### Builtin hook kinds (closed set, v1)

v1 ships a closed set of five builtins, registered by kind name
in `BuiltinHookRegistry::for_kind`. User-authored arbitrary
code is **out of scope**; ADR
[`0024`](0024-wasm-plugin-system.md) is the extension path.

#### `deny_command_pattern`

Pre-only. Matches `tool_name` against `applies_to` (typically
`run_command` from ADR [`0028`](0028-shell-executor-and-test-runners.md)),
inspects an `argv` field on `arguments`, and denies on regex
match.

```yaml
- kind: deny_command_pattern
  applies_to: run_command
  config:
    patterns:
      - '^rm\s+-rf\s+/'
      - '^curl\s+.*\|\s*sh'
      - '^git\s+push\s+.*--force\b'
    reason: "Tenant policy disallows destructive commands; rephrase or use a transactional code change (ADR 0031)."
```

On match, returns `Deny { reason }`. The LLM sees the reason as
the tool result on the next turn.

#### `require_clean_index_before_commit`

Pre-only. Matches `tool_name == "git_commit"` (ADR
[`0030`](0030-git-operations.md)). Inspects the `WorkspaceCtx`
on `HookContext` and denies if the index has untracked or
unstaged changes that the call's `paths` argument does not list.

```yaml
- kind: require_clean_index_before_commit
  applies_to: git_commit
  config:
    allow_untracked: false
```

Rationale: "I committed your dirty cache file" is a top-N user
complaint with weak coding agents.

#### `redact_regex_in_output`

Post-only. Walks the `result` `Value` and replaces regex
matches with a sentinel string. Returns
`HookOutcome::Modify(redacted)`.

```yaml
- kind: redact_regex_in_output
  applies_to: '*'
  config:
    patterns:
      - { regex: '(?i)bearer\s+[A-Za-z0-9._-]+', replace: 'Bearer <REDACTED>' }
      - { regex: 'sk-[A-Za-z0-9]{20,}',          replace: '<REDACTED-API-KEY>' }
```

#### `log_to_artifact`

Post-only. Persists the `(arguments, result)` pair as an ADR
[`0016`](0016-artifact-storage.md) artifact, attaches the
artifact id to the per-task event log, and returns
`HookOutcome::Allow` (does not modify the result the LLM sees).
Used for high-value audit (`run_command`, `git_commit`).

```yaml
- kind: log_to_artifact
  applies_to: 'git_*'
  config:
    retention: 30d
    label: "git-ops-audit"
```

#### `require_a2a_plan_verification`

Post-only. Matches `tool_name == "propose_plan"` (ADR
[`0038`](0038-plan-mode-and-cross-verification.md)). On a
successful `propose_plan` call, dispatches the emitted
`Plan` `DataPart` to the named verifier peers via the existing
delegation path (ADR [`0006`](0006-peer-delegation.md), ADR
[`0007`](0007-remote-a2a-agent-client.md)), applies ADR
[`0038`](0038-plan-mode-and-cross-verification.md)'s
aggregation policy, and:

- on `Approved` — returns `HookOutcome::Allow` (the agent loop
  proceeds to flip `AgentPhase::Plan` → `AgentPhase::Execute`
  per ADR [`0038`](0038-plan-mode-and-cross-verification.md));
- on `RequestChanges` — returns
  `HookOutcome::Modify(verdict_payload)` so the planner sees
  the verifier findings as the tool result and can revise the
  plan on the next turn;
- on `Rejected` — returns `HookOutcome::Deny { reason }` and
  the step terminates per ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)'s
  *Failure modes*.

```yaml
- kind: require_a2a_plan_verification
  applies_to: propose_plan
  config:
    verifiers:
      - agent_ref: ork.agent.plan_verifier.haiku
        weight: 1.0
      - agent_ref: ork.agent.plan_verifier.local
        weight: 0.5
    aggregation: majority         # ADR 0038 AggregationPolicy
    timeout_per_verifier: 60s
    on_unreachable: fail_closed   # ADR 0038 TimeoutPolicy
    require_distinct_verifier_model: true
```

The hook holds **no protocol logic of its own** — it constructs
an ADR [`0038`](0038-plan-mode-and-cross-verification.md)
`PlanVerificationPolicy` from its config and delegates to that
ADR's `PlanCrossVerifier`. ADR [`0038`] *defines* the protocol;
this hook *triggers* it. A workflow step that sets
`plan_verification` directly (ADR
[`0038`](0038-plan-mode-and-cross-verification.md) §`Workflow`)
takes precedence and the hook is skipped — the gate is run
exactly once.

#### Builtin registry

```rust
// crates/ork-agents/src/hooks.rs (continued)

pub struct BuiltinHookRegistry;

impl BuiltinHookRegistry {
    /// Resolve a kind name to a builtin constructor. Returns
    /// `None` for unknown kinds; the loader treats unknown
    /// kinds as a config error (do not silently drop).
    pub fn for_kind(kind: &str) -> Option<HookFactory> { /* ... */ }
}

pub type HookFactory = fn(&HookConfig) -> Result<Arc<dyn AgentHook>, OrkError>;
```

A future ADR may extend the registry surface to include WASM
hooks (ADR [`0024`](0024-wasm-plugin-system.md)) under a
distinct namespace (e.g. `wasm:my_org.my_hook`); v1 rejects
non-builtin kinds at load time.

### Integration in `LocalAgent`

The hook chain runs inside the existing tool-call seam in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs).
`execute_tool_call` is wrapped:

```rust
async fn execute_tool_call_with_hooks(
    chain: &HookChain,
    hook_ctx: &HookContext,
    tools: Arc<dyn ToolExecutor>,
    ctx: AgentContext,
    call: ToolCall,
    max_tool_result_bytes: usize,
    semaphore: Arc<Semaphore>,
) -> Result<(String, String, bool), OrkError> {
    // 1. Pre-tool-call chain.
    let mut args = call.arguments.clone();
    match chain.run_pre(hook_ctx, &call.name, &call.id, &mut args).await? {
        HookOutcome::Allow => {}
        HookOutcome::Modify(new_args) => args = new_args,
        HookOutcome::Deny { reason, .. } => {
            // Tool never runs. The reason becomes the tool result.
            let payload = hook_deny_payload(&call.name, &reason);
            return Ok((call.id, payload, false));
        }
    }

    // 2. The existing executor call, with possibly-modified args.
    let executed = ToolCall { arguments: args, ..call };
    let (id, content, truncated) =
        execute_tool_call(tools, ctx, executed, max_tool_result_bytes, semaphore).await?;

    // 3. Post-tool-call chain.
    let mut result: Value = serde_json::from_str(&content)
        .unwrap_or(Value::String(content.clone()));
    match chain.run_post(hook_ctx, &call.name, &id, &mut result).await? {
        HookOutcome::Allow => Ok((id, content, truncated)),
        HookOutcome::Modify(new_result) => {
            let serialized = serde_json::to_string(&new_result)
                .map_err(|e| OrkError::Internal(format!("serialize hook result: {e}")))?;
            Ok((id, serialized, truncated))
        }
        HookOutcome::Deny { reason, .. } => {
            let payload = hook_deny_payload(&call.name, &reason);
            Ok((id, payload, false))
        }
    }
}
```

A `Deny` reason is returned to the LLM via the existing
`ChatMessage::tool` path so the model sees it on the next turn
and can adapt — this matches the ADR [`0010`](0010-mcp-tool-plane.md)
philosophy that recoverable tool failures stay in the tool result,
not as fatal step errors.

The chain is resolved once per step, before the tool-call
loop begins, by walking tenant config, team config (no-op
until ADR [`0043`]), and persona config in that order. Hook
construction failures (e.g. unknown kind, malformed config)
fail the *step*, not silently degrade.

### Hooks vs RBAC

RBAC denies first. The order at every tool dispatch is:

```
ToolCall
  → ScopeChecker (ADR 0021)        ← platform-enforced authorization
  → HookChain.pre_tool_call (this) ← user-authored guardrails
  → ToolExecutor::execute (ADR 0011)
  → HookChain.post_tool_call (this)
```

A hook **cannot grant** access an RBAC scope denied. A hook
**can deny** access RBAC permitted. This is the correct
asymmetry: RBAC is the floor (the platform decides who is
allowed to do what), hooks are the ceiling an operator chooses
to lower further.

If RBAC and hooks both deny, RBAC's denial is the one logged
to the audit stream (it's the more privileged failure); the
hook chain is short-circuited.

### Hooks vs verifier (ADR 0025)

The verifier port (ADR
[`0025`](0025-typed-output-validation-and-verifier-agent.md))
fires on a step's typed *output* once the producing step has
finished. Hooks fire on individual *tool-calls* during the
step. The two are complementary, not redundant: a step may emit
20 tool calls (each with hooks) and produce one typed output
(verified once at the end). Hooks cannot replace the verifier
(they don't see the typed output) and the verifier cannot
replace hooks (it doesn't see the per-call argv).

### Hooks vs cross-verification (ADR 0038)

Hooks **trigger** ADR
[`0038`](0038-plan-mode-and-cross-verification.md)'s plan
cross-verification protocol via the
`require_a2a_plan_verification` builtin. ADR
[`0038`](0038-plan-mode-and-cross-verification.md) **defines**
the protocol — plan schema, verdict schema, aggregation,
diversity, HITL composition. The hook is one of two ways to
fire that gate; the other is `WorkflowStep::plan_verification`
(ADR [`0038`](0038-plan-mode-and-cross-verification.md) §
*Verifier peers are selected, in order*). The gate runs at most
once per `propose_plan` call.

### Observability

Every hook decision (Allow / Deny / Modify) emits an event on
ADR [`0022`](0022-observability.md)'s per-task event log:

| Event kind | Fired when |
| ---------- | ---------- |
| `hook.pre.allow` | `pre_tool_call` returned `Allow` |
| `hook.pre.modify` | `pre_tool_call` returned `Modify` (carries diff) |
| `hook.pre.deny` | `pre_tool_call` returned `Deny` (carries reason) |
| `hook.post.allow` | `post_tool_call` returned `Allow` |
| `hook.post.modify` | `post_tool_call` returned `Modify` |
| `hook.post.deny` | `post_tool_call` returned `Deny` |
| `hook.error` | hook returned `Err(OrkError)` |

Each event carries `tenant_id`, `team_id`, `persona_id`,
`agent_id`, `task_id`, `tool_call_id`, `tool_name`,
`hook_kind`, `scope` (`tenant | team | persona`), and a
finding id where applicable. ADR
[`0022`](0022-observability.md)'s `BudgetMonitor` gains a
`hook_dispatch_count` counter so a runaway hook chain is
visible in dashboards.

### Out of scope

- **Dynamic hook code.** No embedded scripting (Lua, JS,
  Starlark) in v1. Hooks are a closed builtin set.
- **WASM plugin hooks.** ADR
  [`0024`](0024-wasm-plugin-system.md) is the extension path.
  When 0024 lands, a `wasm:` kind namespace plugs into the
  registry above; v1 rejects unknown kinds.
- **Per-tool-call cost / budget enforcement.** A
  `BudgetGuard` hook is desirable but belongs with ADR
  [`0022`](0022-observability.md)'s budget surface, not here.
- **Hook authoring UI.** Hooks are configured in tenant /
  team / persona config files in v1; a web-UI editor (ADR
  [`0017`](0017-webui-chat-client.md)) is a follow-up.
- **Hooks on A2A peer responses.** A hook on
  `agent_call`'s *result* is just `post_tool_call`; a hook on
  the wire frames between mesh agents is not — that belongs
  with ADR [`0020`](0020-tenant-security-and-trust.md)'s
  trust plane.

## Acceptance criteria

- [ ] Trait `AgentHook` defined at
      `crates/ork-agents/src/hooks.rs` with the
      `pre_tool_call` / `post_tool_call` signatures shown in
      `Decision`, default-impl `Allow`.
- [ ] Types `HookOutcome<T>`, `PreToolCall<'a>`,
      `PostToolCall<'a>`, `HookContext`, `HookConfig`,
      `HookPhaseMask`, `HookChain`, `HookScope` defined in the
      same module with serde derives where shown.
- [ ] `HookContext::team_id: Option<TeamId>` defaults to
      `None`; the type compiles when `TeamId` is the
      placeholder `pub struct TeamId(pub String);` until ADR
      [`0043`] (forthcoming) lands.
- [ ] `BuiltinHookRegistry::for_kind` registers exactly the
      five v1 kinds: `deny_command_pattern`,
      `require_clean_index_before_commit`,
      `redact_regex_in_output`, `log_to_artifact`,
      `require_a2a_plan_verification`. Unknown kinds return
      `None` and the loader fails the step with
      `OrkError::Validation("unknown_hook_kind")`.
- [ ] `HookChain::run_pre` runs tenant → team → persona;
      `run_post` runs persona → team → tenant — verified by
      `crates/ork-agents/tests/hooks_chain_order.rs::pre_runs_outermost_first`
      and `::post_runs_innermost_first`.
- [ ] First `Deny` short-circuits the chain — verified by
      `crates/ork-agents/tests/hooks_chain_order.rs::tenant_deny_blocks_persona_modify`.
- [ ] `Modify` outcomes accumulate — verified by
      `crates/ork-agents/tests/hooks_chain_order.rs::two_modifies_compose`.
- [ ] `LocalAgent` invokes the chain at the
      `execute_tool_call` seam in
      [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs);
      `Deny` payload reaches the LLM as a `ChatMessage::tool`
      — verified by
      `crates/ork-agents/tests/hooks_local_agent.rs::deny_returns_reason_to_llm`.
- [ ] `Modify` in `pre_tool_call` rewrites the arguments seen
      by the executor — verified by
      `crates/ork-agents/tests/hooks_local_agent.rs::pre_modify_rewrites_args`.
- [ ] `Modify` in `post_tool_call` rewrites the tool result
      seen on `history` — verified by
      `crates/ork-agents/tests/hooks_local_agent.rs::post_modify_rewrites_result`.
- [ ] `deny_command_pattern` builtin matches argv against a
      regex list and denies on match — verified by
      `crates/ork-agents/tests/hooks_deny_command.rs::denies_rm_rf`
      and `::allows_unrelated_argv`.
- [ ] `require_clean_index_before_commit` denies a
      `git_commit` call when the workspace has unstaged or
      untracked changes outside the call's `paths` — verified
      by
      `crates/ork-agents/tests/hooks_clean_index.rs::denies_dirty_index`.
- [ ] `redact_regex_in_output` rewrites matched substrings in
      a tool result — verified by
      `crates/ork-agents/tests/hooks_redact.rs::redacts_bearer_token_in_nested_value`.
- [ ] `log_to_artifact` persists `(arguments, result)` as an
      ADR [`0016`](0016-artifact-storage.md) artifact and
      attaches the artifact id to the event log — verified by
      `crates/ork-agents/tests/hooks_log_to_artifact.rs::persists_and_emits_event`.
- [ ] `require_a2a_plan_verification` constructs an ADR
      [`0038`](0038-plan-mode-and-cross-verification.md)
      `PlanVerificationPolicy` from its YAML config and
      dispatches via the existing `PlanCrossVerifier` impl;
      `Approve` → `Allow`, `RequestChanges` → `Modify`,
      `Reject` → `Deny` — verified by
      `crates/ork-agents/tests/hooks_plan_verification.rs::approve_allows`,
      `::request_changes_returns_findings_to_llm`,
      `::reject_terminates_call`.
- [ ] When a `WorkflowStep::plan_verification` is set
      directly, the hook is skipped (gate runs exactly once)
      — verified by
      `crates/ork-agents/tests/hooks_plan_verification.rs::workflow_policy_wins`.
- [ ] RBAC denies before hooks run — verified by
      `crates/ork-agents/tests/hooks_rbac_precedence.rs::rbac_deny_short_circuits_before_hooks`.
- [ ] Hook decisions land on the per-task event log per the
      table in `Decision`, with `tenant_id`, `team_id`,
      `persona_id`, `tool_name`, `hook_kind`, `scope` — verified
      by
      `crates/ork-agents/tests/hooks_observability.rs::events_emitted_with_required_fields`.
- [ ] Team-scope config with no `TeamId` resolved is dropped
      at load time with a `tracing::info!` audit and an
      `OrkError`-free path — verified by
      `crates/ork-agents/tests/hooks_team_placeholder.rs::team_scope_no_op_until_0043`.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for
      `0039` added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended
      after implementation lands.

## Consequences

### Positive

- A tenant gets a sharp, cheap-to-configure guardrail
  (`deny_command_pattern`) without writing an RBAC scope
  policy. The 80% case is a regex; treating it as one matches
  what operators reach for.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  Plan→Execute gate has a defined enforcement point. Without
  this ADR, the gate would either have to be hard-wired into
  `LocalAgent` (no opt-out) or fragmented across persona /
  workflow config; the hook surface is the natural seam and
  reuses the per-call observability already wired here.
- The hook chain composes cleanly with RBAC: RBAC is the
  floor, hooks are the ceiling; neither can subvert the other.
  Operators can reason about each layer in isolation.
- The trait surface is forward-compatible with WASM plugins
  (ADR [`0024`](0024-wasm-plugin-system.md)). A future
  `wasm:my_org.redact` plugin implements the same trait and
  drops into the same chain — no per-extension special-case in
  `LocalAgent`.
- Audit-friendly: every guardrail decision is a typed event,
  not an `info!` log line, so dashboards (ADR
  [`0022`](0022-observability.md)) can answer "how often did
  hook X fire and on which tool" without a regex over logs.

### Negative / costs

- Three scopes (tenant, team, persona) plus pre/post phases is
  a small but real configuration surface. Operators will pick
  a scope and it will be *almost* right; misplaced hooks
  (tenant scope when team scope was intended) produce
  surprising deny-everywhere behaviour. Mitigation: the event
  log carries `scope` and `hook_kind` so the source of any
  deny is visible.
- A misconfigured `redact_regex_in_output` pattern can corrupt
  a tool result the LLM was about to act on — a redaction
  rule that mangles a JSON value will look to the LLM like a
  malformed response. Mitigation: hooks operate on
  `serde_json::Value` so structural redaction (drop a field)
  is preferred over byte-level regex; documentation calls this
  out.
- Five builtins is a small set on day one but every kind we
  add is a stable surface we own forever — reviewers must
  resist the temptation to add a sixth without superseding
  this ADR.
- A poorly-implemented hook can stall the tool-call loop. v1
  has no per-hook timeout (timeouts are part of the executor
  contract); a hook that awaits forever blocks the step until
  the agent task is cancelled. Mitigation: the
  `require_a2a_plan_verification` builtin (the only one that
  performs network I/O) takes its timeout from ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)'s
  policy; other v1 builtins are pure-CPU. A
  `hook_dispatch_count` counter on `BudgetMonitor` surfaces
  runaways.
- Team scope is no-op until ADR [`0043`] lands; team-scope
  config is silently dropped (with an info log). An operator
  who configures team-scope hooks before 0043 will get a
  weaker guardrail than they expected. Mitigation: load-time
  log; the team-scope CLI flag (when 0043 lands) carries a
  deprecation note for any pre-0043 ergonomics that may have
  shipped.
- Hooks add a per-tool-call latency floor (one chain run per
  call). For pure-CPU builtins this is microseconds; for
  `require_a2a_plan_verification` it is dominated by the A2A
  round-trip and is the cost ADR
  [`0038`](0038-plan-mode-and-cross-verification.md) already
  budgets.

### Neutral / follow-ups

- ADR [`0021`](0021-rbac-scopes.md)'s `ScopeChecker` and the
  hook chain coexist; no change to RBAC's surface. Reviewers
  must not be tempted to fold one into the other.
- ADR [`0022`](0022-observability.md) gains six event kinds
  and one counter; otherwise unchanged.
- ADR [`0024`](0024-wasm-plugin-system.md) gains a clear
  extension shape: WASM plugins implement `AgentHook` (over
  the WASM ABI) and register under a `wasm:` kind namespace.
  Spec'd in 0024, not here.
- ADR [`0033`](0033-coding-agent-personas.md)'s
  `CodingPersona` gains a `hooks: Vec<HookConfig>` field;
  built-in personas in `0033` get the hooks they need (e.g.
  `solo_coder` ships with `require_clean_index_before_commit`,
  `architect` with `require_a2a_plan_verification` when team
  flow).
- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  `PlanVerificationPolicy` becomes the construction target
  for the `require_a2a_plan_verification` builtin; no other
  cross-link is needed.
- ADR [`0043`] (forthcoming team identity) lands the team
  scope's actual implementation; this ADR's `TeamId`
  placeholder evaporates at that point.
- ADR [`0045`] (forthcoming team orchestrator) consumes the
  three-scope composition: a tenant admin sets the floor, the
  team lead adds team-wide hooks, the persona author bundles
  persona-specific hooks. No further wire-up needed in 0045.
- A future ADR may add a sixth builtin
  (`require_clean_workspace_before_test`) if dogfooding turns
  it up; it would supersede this one or amend via a
  follow-up.

## Alternatives considered

- **Fold hooks into RBAC.** Treat `deny_command_pattern` as
  an RBAC scope policy with a regex predicate. Rejected:
  RBAC's authority model is "principal X may do Y on Z";
  bolting on regex predicates overloads the scope grammar
  and makes the audit chain ambiguous (was this denied by an
  admin or by the agent author?). Hooks have a different
  audience (user-authored vs. platform-enforced) and a
  different feedback loop (the LLM sees the deny reason).
- **Single-scope hooks (persona only).** Cheaper to build,
  but tenant admins routinely want a global `git push --force`
  block. Persona-only would force every persona author to
  remember to add it; tenant scope means it's set once and
  cannot be loosened below. Rejected.
- **Allow hooks to *override* RBAC denials.** Fashionable in
  some local agent harnesses (let the user opt out of safety).
  Rejected: RBAC is the platform's authority floor; allowing
  user-config to lift it inverts the trust model. If an
  operator wants laxer RBAC, they edit RBAC, not a hook.
- **Run hooks in parallel inside a scope.** Slightly faster
  for pure-CPU hooks. Rejected: deny-wins-first is much
  easier to reason about with sequential semantics, and the
  contention is on tool-call latency (already dominated by
  the executor itself, not the hook chain).
- **Symmetric outcome where both pre and post can return any
  of allow/deny/modify.** Adopted as written; the
  alternative (post can only modify or allow, not deny) was
  considered and rejected because `redact_regex_in_output`
  occasionally finds *too much* (an entire tool result
  matched as a secret) and the natural response is "drop
  this result, return a deny payload". A post-deny that
  cannot deny would have to reach into the result and synthesise
  one, awkwardly.
- **Embed a scripting language (Lua / Starlark) for hook
  bodies in v1.** Rejected: the v1 builtin set covers the
  observed 80% case, the WASM extension path (ADR
  [`0024`](0024-wasm-plugin-system.md)) is already chosen,
  and embedding two scripting layers is two layers to
  sandbox, fuzz, and version. Builtins now; WASM later.
- **Make `propose_plan` cross-verification a workflow step,
  not a hook.** Workflows already carry
  `WorkflowStep::plan_verification` (ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)); does
  the hook duplicate it? Rejected: ad-hoc agent runs (REPL,
  `ork chat`) have no workflow YAML, but operators still
  want cross-verification. The hook is the
  workflow-independent path; the workflow path is the
  in-DAG path; both call into the same gate.
- **Composition order persona → team → tenant (innermost
  first).** Considered to mirror how config layers usually
  resolve (most-specific wins). Rejected: deny-wins is the
  semantic that matters, and "tenant cannot be loosened
  below" requires tenant-first evaluation. Innermost-first
  would let a persona allow before a tenant could deny —
  exactly the trust inversion we are avoiding.
- **Separate trait per phase (`PreToolCallHook` vs
  `PostToolCallHook`).** Cleaner type-wise but doubles the
  registry surface and complicates a hook that wants to
  match both phases (most non-trivial ones do). Rejected;
  one trait, two methods, default-impl `Allow`.

## Affected ork modules

- New: [`crates/ork-agents/src/hooks.rs`](../../crates/ork-agents/) —
  trait, outcome, context, config, chain, builtin registry,
  and the five builtin impls (one per kind).
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  — wrap `execute_tool_call` with `HookChain::run_pre` /
  `run_post`; resolve the chain once per step before the
  tool-call loop.
- [`crates/ork-agents/src/lib.rs`](../../crates/ork-agents/src/lib.rs)
  — re-export the public hook surface.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/) — if
  `EventSink`, `ArtifactStore`, `PeerDispatcher` are not
  already port-shaped, add the minimal traits the hook
  context depends on (no new transport).
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — `propose_plan` registration carries a
  `read_only_class: ReadOnly` (per ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)) and a
  call-site annotation that the
  `require_a2a_plan_verification` hook is the natural pairing.
- [`crates/ork-core/src/workflow/plan_gate.rs`](../../crates/ork-core/) —
  no new code; the `require_a2a_plan_verification` builtin
  consumes ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  `PlanCrossVerifier` trait unchanged.
- [`crates/ork-agents/src/personas/`](../../crates/ork-agents/) —
  each persona shipped by ADR
  [`0033`](0033-coding-agent-personas.md) gains its
  default `hooks: Vec<HookConfig>`.
- [`crates/ork-cli/src/`](../../crates/ork-cli/) — tenant /
  persona config files gain a `hooks` section; the loader
  builds `HookChain` with `BuiltinHookRegistry`.
- [`workflow-templates/`](../../workflow-templates/) — sample
  team / solo templates show `hooks:` blocks at the right
  scopes.
- [`docs/adrs/README.md`](README.md) — ADR index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass
on the implementation diff (see [`AGENTS.md`](../../AGENTS.md)
§3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Claude Code | `PreToolUse` / `PostToolUse` hooks (settings.json hooks block) | `AgentHook::pre_tool_call` / `post_tool_call` |
| opencode | pre/post tool-call hook config | `HookConfig` + closed builtin set |
| Aider | `--pre-commit` shell hook | `require_clean_index_before_commit` builtin |
| Cursor | per-rule "deny tool" config | `deny_command_pattern` builtin |
| Cline | mode-conditional tool gating | persona-scope `HookConfig` |
| Fly.io / Cloudflare middleware patterns | scope chains with deny-wins composition | tenant → team → persona chain order |
| ork's own [`AGENTS.md`](../../AGENTS.md) §3 `code-reviewer` gate | post-implementation reviewer mandatory | runtime per-call analogue: `log_to_artifact` for high-value calls |

## Open questions

- **Per-hook timeout.** v1 has no per-hook timeout. The only
  network-doing builtin (`require_a2a_plan_verification`)
  inherits ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  policy; the others are pure CPU. Should v1.1 add a uniform
  per-hook timeout (e.g. 5s) as a defence against
  user-authored WASM hooks misbehaving? Defer.
- **Hook composition with MCP-server hooks.** ADR
  [`0010`](0010-mcp-tool-plane.md) MCP servers may grow their
  own pre/post hook surface. Should ork's hook chain wrap MCP
  tool calls too, or only native ones? v1 wraps both
  (the seam is at the agent loop, not at the executor); MCP
  server-side hooks compose underneath, not coordinated.
  Revisit if MCP-side hooks become idiomatic.
- **`Modify` audit fidelity.** `hook.pre.modify` events
  carry the diff between original and modified arguments;
  for large arguments this can balloon the event log.
  Consider hashing + storing the diff as an artifact instead.
  Defer to ADR [`0022`](0022-observability.md) follow-up.
- **Hook ordering UI.** Within a scope, hooks run in config
  order. There is no visual editor for that order in v1;
  reorder by editing YAML. Web UI affordance is a follow-up.
- **`require_a2a_plan_verification` on non-`propose_plan`
  tools.** A future variant might gate `apply_patch` on a
  separate code-review verifier. The hook surface
  accommodates it; v1 ships only the `propose_plan` binding.

## References

- A2A spec: <https://github.com/google/a2a>
- Anthropic, Claude Code hooks docs:
  <https://docs.claude.com/en/docs/claude-code/hooks>
- opencode hooks reference:
  <https://opencode.ai/docs/hooks>
- Aider pre-commit hook:
  <https://aider.chat/docs/usage/lint-test.html>
- Related ADRs: [`0011`](0011-native-llm-tool-calling.md),
  [`0021`](0021-rbac-scopes.md),
  [`0022`](0022-observability.md),
  [`0024`](0024-wasm-plugin-system.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0027`](0027-human-in-the-loop.md),
  [`0033`](0033-coding-agent-personas.md),
  [`0038`](0038-plan-mode-and-cross-verification.md),
  0042 (forthcoming), 0043 (forthcoming), 0045 (forthcoming).
