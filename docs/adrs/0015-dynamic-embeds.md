# 0015 — Dynamic embeds

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0011, 0013, 0014, 0016

## Context

ork's prompt templates use a single resolution pass through `resolve_template` in [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs): `{{step_id.output}}`, `{{input.field}}`, `{{this.iteration}}` get string-substituted before the prompt is sent to the LLM. That covers static templating but doesn't support:

- Inline computation (`«math:1024 * 0.7 | format:bytes»`);
- Dynamic data lookups (`«artifact_meta:report.pdf»`, `«status_update:task-123»`);
- Streaming-time embeds resolved as the agent emits text (e.g. an LLM that says `«artifact_content:report.pdf»` mid-stream).

SAM ships exactly this with its **dynamic embed resolver** ([`common/services/embeds/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/services/embeds)) — the `«type:expression | format»` syntax is one of SAM's defining UX features and is heavily used by gateways to render artifact links, status badges, math results, and timestamps.

ork needs the same capability for parity, and to match users' expectations after they've seen SAM.

## Decision

ork **introduces a dynamic embed resolver** in a new module `crates/ork-core/src/embeds/` that lives **alongside** the existing `{{template}}` resolver, not replacing it. The two compose:

```
prompt_template → {{template}} resolver → «embed» early-phase resolver → LLM call
                                                                      ↓
                                                                 stream tokens
                                                                      ↓
                                                       «embed» late-phase resolver → user
```

### Embed syntax

```
«<type>:<expression> | <format>»
```

- `<type>` — the embed handler id (e.g. `math`, `datetime`, `uuid`, `artifact_meta`, `artifact_content`, `status_update`).
- `<expression>` — handler-specific argument; usually a single token (artifact name, expression, key) but may be multi-line.
- `<format>` — optional format hint passed to the handler (e.g. `bytes`, `iso8601`, `markdown`, `json`).
- Delimiters use the angle quotation marks `«` and `»` (U+00AB, U+00BB), exactly matching SAM, so embeds copy-paste between systems.

A handler is identified by `type`. Unknown types produce no substitution and emit a warning (the literal `«…»` survives, so authors notice).

### Handler trait

```rust
// crates/ork-core/src/embeds/mod.rs

#[async_trait::async_trait]
pub trait EmbedHandler: Send + Sync {
    fn type_id(&self) -> &'static str;
    fn phase(&self) -> EmbedPhase;     // Early | Late | Both

    async fn resolve(&self, expr: &str, format: Option<&str>, ctx: &EmbedContext)
        -> Result<EmbedOutput, EmbedError>;
}

pub enum EmbedPhase { Early, Late, Both }

pub enum EmbedOutput {
    Text(String),
    /// For late-phase: replace embed with a chunk of streaming output (e.g. artifact_content),
    /// possibly multiple parts (e.g. text + image).
    Parts(Vec<Part>),
}

pub struct EmbedContext<'a> {
    pub tenant_id: TenantId,
    pub task_id: Option<TaskId>,
    pub artifact_store: Arc<dyn ArtifactStore>,    // ADR 0016
    pub a2a_repo: Arc<dyn A2aTaskRepository>,      // ADR 0008
    pub now: DateTime<Utc>,
    pub depth: usize,                              // for recursion control
}
```

### Built-in handlers (parity set)

| Type id | Phase | Purpose | Format hints |
| ------- | ----- | ------- | ------------ |
| `math` | Early | Evaluate a numeric expression (uses `evalexpr` crate) | `bytes`, `percent`, `int`, `usd` |
| `datetime` | Early | Current/relative time | `iso8601`, `unix`, `human`, `<strftime>` |
| `uuid` | Early | Generate a UUID v4 | none |
| `artifact_meta` | Both | Lookup metadata of a named artifact in `ArtifactStore` (ADR [`0016`](0016-artifact-storage.md)) | `json`, `name`, `size`, `mime` |
| `artifact_content` | Late | Inline an artifact's content; if binary, emit as `Part::File` | `text`, `markdown`, `image`, `json` |
| `status_update` | Late | Latest A2A status event for a task id | `summary`, `state`, `json` |
| `var` | Early | Read from `EmbedContext.variables` (a per-call map populated by callers) | none |
| `secret` | Early | Read from a per-tenant secret store (ADR [`0020`](0020-tenant-security-and-trust.md)) — auditable, restricted by RBAC | none |

These exactly match SAM's built-in set documented in [`common/services/embeds/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/services/embeds).

### Phases

- **Early** — resolved on the **input** to the LLM call (i.e. before sending the request). After `{{template}}` resolution. Produces a fully resolved prompt with no embed syntax for early-phase types.
- **Late** — resolved on the **output** of the LLM call as it streams back, before delivery to the gateway/SSE client. The LLM may emit `«artifact_content:report.pdf»` and the resolver expands it into a multi-part chunk — exactly SAM's behaviour.

`Both` handlers (e.g. `artifact_meta`) are useful in either phase.

### Late-phase streaming integration

Late-phase resolution is implemented as a `tokio` stream transformer wrapping the `AgentEvent` stream from `Agent::send_stream` (ADR [`0002`](0002-agent-port.md)):

```rust
let stream = local_agent.send_stream(ctx, msg).await?;
let stream = LateEmbedResolver::new(handlers, embed_ctx).wrap(stream);
return Ok(Box::pin(stream));
```

The resolver buffers small windows of text deltas to find embed delimiters across chunk boundaries, emits `Part::File` for image/binary embeds, and forwards everything else verbatim. This composes with [`crates/ork-api/src/routes/a2a.rs`](../../crates/ork-api/src/routes/) (ADR [`0008`](0008-a2a-server-endpoints.md)) and gateway adapters (ADR [`0013`](0013-generic-gateway-abstraction.md)) without either knowing about embeds.

### Recursion and limits

- `EmbedContext.depth` increments on each recursive resolve; capped at `max_embed_depth` (default 4). Prevents `«var:loop»` cycles.
- `«embed_*»` resolution itself is bounded by `max_embeds_per_request` (default 64).
- Late-phase output cap: `max_late_embed_output_bytes` (default 1 MB) — over-sized output truncates to an artifact reference.

### Plugin embed handlers

ADR [`0014`](0014-plugin-system.md) lets plugins register additional handlers via `reg.register_embed_handler(type_id, handler)`. Plugins must declare any required RBAC scopes; the resolver enforces them.

### Backwards compatibility with existing `{{...}}`

The existing `resolve_template` ([`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)) is unchanged in behaviour. Embeds use different delimiters, so old templates keep working. New templates may freely mix:

```yaml
prompt_template: |
  Run «math:{{input.shards}} * 1024»  bytes through the analyser.
  Save the result to «artifact_meta:report-{{input.run_id}}.json | name».
```

`{{...}}` runs first; then `«...»`.

## Consequences

### Positive

- ork users get the SAM embed UX without learning a new syntax (embeds are copy-paste compatible).
- Late-phase artifact embeds let the LLM produce rich responses (markdown with inline images) without bespoke gateway code.
- Plugin-registered handlers extend the resolver without core changes (e.g. `«jira_ticket:ABC-123»`).

### Negative / costs

- Late-phase resolver complicates the streaming path (buffering across deltas, Part-emission). Mitigated with focused unit tests around delimiter parsing.
- Two template syntaxes (`{{...}}` and `«...»`) might confuse newcomers. Documented up front.
- Embed handlers can do expensive lookups (artifact reads, DB hits); a single prompt with many embeds can fan out. Mitigated by per-handler caching (`artifact_meta` cache: 60s) and the `max_embeds_per_request` cap.

### Neutral / follow-ups

- ADR [`0016`](0016-artifact-storage.md) defines the `ArtifactStore` that several handlers depend on.
- A future ADR may unify the two resolvers behind a single template engine if a real-world need emerges.
- We may publish a small `ork-embeds` standalone crate so non-ork tools (e.g. internal CLI helpers) can use the same syntax.

## Alternatives considered

- **Bake embeds into the existing `{{...}}` resolver.** Rejected: would conflict with handlebars-style templates that ork or its users may adopt; embed semantics are different (typed dispatch, late-phase).
- **Use Tera or Handlebars as the template engine.** Rejected: adds a heavy dep for a small surface; embeds need streaming late-phase resolution that template engines don't support.
- **Resolve embeds only at the gateway boundary.** Rejected: agents-without-gateway (peer-to-peer A2A) would lose the feature.
- **No late-phase embeds — only early.** Rejected: the killer feature (LLM emits `«artifact_content:...»` mid-stream and gets inline rendering) requires late phase.

## Affected ork modules

- New module: `crates/ork-core/src/embeds/{mod.rs,parser.rs,handlers/}` — registry, parser, builtin handlers.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — `resolve_template` chains into `EmbedResolver::resolve_early` after `{{...}}`.
- New: `crates/ork-core/src/streaming/late_embed.rs` — late-phase stream wrapper.
- [`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/) — wrap A2A stream output through `LateEmbedResolver` (ADR [`0008`](0008-a2a-server-endpoints.md)).
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/ports/) — re-export `EmbedHandler` for plugin authors via `ork-plugin-api` (ADR [`0014`](0014-plugin-system.md)).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Embed syntax `«type:expr | fmt»` | [`common/services/embeds/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/services/embeds) | Identical syntax in `crates/ork-core/src/embeds/parser.rs` |
| Built-in `math`, `datetime`, `uuid`, `artifact_meta`, `artifact_content`, `status_update` | SAM same module | Same handlers in `handlers/` directory |
| Early/late resolution phases | SAM gateway/agent code | `EmbedPhase` enum + stream wrapper |
| Plugin embed handler | SAM plugin convention | `reg.register_embed_handler` (ADR [`0014`](0014-plugin-system.md)) |

## Open questions

- Should `«» ` syntax allow nested embeds (e.g. `«artifact_meta:«var:current_artifact»»`)? Decision: yes, but bounded by `max_embed_depth`.
- Format hint vocabulary across handlers should be standardised. We start with the SAM-aligned set above and add a "format hint" registry as needed.
- Markdown vs HTML rendering of `artifact_content` — defer the renderer choice to the gateway; embeds emit `Part::Text` with mime hint.
- **Implementation deferrals (this PR):** `artifact_meta` / `artifact_content` (need ADR-0016 `ArtifactStore`); `secret` (ADR-0020); `reg.register_embed_handler` / public plugin surface (wait for ADR-0024 WASM plugin runtime). `«uuid»` / `«datetime»` with no `:` use `parse_embed_body`’s no-colon branch (type only, empty expr) — see tests.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Major | `«uuid»` bodies lack `:`; original `parse_embed_body` only accepted `type:expr`, so zero-arg embeds were treated as malformed. | Fixed: `parse_embed_body` now accepts a single token (type id, empty expr). |
| Minor | `resolve_template` lives in `workflow/template.rs`, not `engine.rs`; call chain is from `engine.rs` after `resolve_template()`. | Acknowledged: ADR "Affected" wording left as high-level; implementation wires `resolve_early` at engine call sites. |

## References

- SAM embeds: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/services/embeds>
- `evalexpr` crate (math handler): <https://crates.io/crates/evalexpr>
