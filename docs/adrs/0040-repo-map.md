# 0040 — Repo map for code-aware context priming

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0010, 0011, 0028, 0029, 0032, 0033, 0034, 0037, 0038, 0041, 0043, 0045
- **Supersedes:** —

## Context

A coding agent that has never seen a repository before walks into the
same wall every time: where do I look, and what is in scope? ork's
read-only surface answers part of that question — `list_tree`,
`code_search`, `read_file` (all in
[`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs))
let an agent crawl the tree on demand — but the agent has to pay one
tool round-trip per directory to discover it, and a `read_file` has
to be issued before any one symbol is in the prompt. That is fine
for frontier models with 200 K-token context windows; it is
catastrophic for the weak local models ADR
[`0034`](0034-per-model-capability-profiles.md) targets.

Three concrete numbers frame the gap:

- A medium ork-style workspace (10–40 crates, 200–600 source files,
  60–250 K LoC) has 30–80 K tokens of source bodies. A `qwen-2.5-coder-7b`
  with an 8 K context cannot hold even one crate verbatim, let alone
  the workspace.
- Without a navigable index the model emits exploratory `list_tree` /
  `read_file` calls — typically 8–20 of them — before it can write a
  single edit. ADR [`0011`](0011-native-llm-tool-calling.md)'s tool
  loop already pays per-iteration LLM latency on every one.
- Plan cross-verification (ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)) needs a
  `plan_verifier` peer to ground its verdict in *what actually exists
  in the repo right now*. A verifier asked "does symbol `X` still
  exist?" without a map either re-crawls the tree (slow, expensive,
  and racy with concurrent edits) or hallucinates a yes/no answer.

Aider's repo-map pattern is the load-bearing prior art here:
*directory tree plus top-level symbol signatures per file* — a
navigable, body-free index a small model can ingest in 1–4 K tokens
that gives it enough orientation to ask for the right `read_file`
next, and gives a verifier a structural ground truth without
re-crawling. opencode, Cline, Cursor, and Claude Code all ship a
variant of the same surface.

ork has every prerequisite to land it without re-deriving primitives:

- ADR [`0029`](0029-workspace-file-editor.md)'s `WorkspaceHandle` and
  `WorkspaceEditor` already own the working tree's lifecycle and
  every write that mutates it.
- ADR [`0028`](0028-shell-executor-and-test-runners.md)'s shell
  executor and ADR [`0030`](0030-git-operations.md)'s git operations
  own every other source of changes (branch checkouts, merges, resets).
  Together with the editor they cover the full set of "the tree just
  changed" events the cache has to invalidate on.
- ADR [`0037`](0037-lsp-diagnostics.md) already establishes the
  pattern of *workspace-keyed, sub-agent-shared* state in
  `ork-integrations`: one rust-analyzer per `(tenant_id,
  workspace_id)`, reused across every agent in that workspace,
  including the nested sub-agents ADR [`0041`] introduces. The repo
  map's lifecycle should mirror that exactly.
- ADR [`0011`](0011-native-llm-tool-calling.md) gives us the native
  tool seam to surface the map to the LLM as `get_repo_map`, parallel
  to ADR [`0037`](0037-lsp-diagnostics.md)'s `get_diagnostics`.
- ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
  persona is a concrete, named consumer waiting on this — it is the
  reason the cache key must be agent-agnostic: a verifier on a
  different model, possibly a different tenant's hosted frontier
  model, must be able to consume *the same* map the planner
  consumed.

What the repo map deliberately is **not**:

- It is **not** the agent-memory store. ADR
  [`0032`](0032-agent-memory-and-context-compaction.md) owns durable,
  cross-task knowledge ("this repo uses tokio with `full` features"
  is a *memory*, not a map artefact). The map is short-term context
  priming for one task on the working tree as it stands *right now*.
- It is **not** the team-wide design index. ADR [`0043`] (planned)
  owns the durable "what decisions has this team made" surface
  consumed by architects. The map is structural, not semantic — it
  surfaces *what symbols exist*, not *why they exist*.
- It is **not** a call-graph or semantic-search index. Both are
  follow-ups that compose on top of the symbol table this ADR
  produces; locking either into v1 would over-commit the wire shape.

## Decision

ork **introduces** a `RepoMap` port in `ork-core`, a `LocalRepoMap`
implementation in `ork-integrations` backed by `tree-sitter` for
language-agnostic top-level symbol extraction, a workspace-keyed
cache layered over `ork-cache`, and a native `get_repo_map` tool
registered through
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs).
Out-of-the-box grammars are **Rust**, **Python**, **TypeScript /
JavaScript**, and **Go**. The map is workspace state, not agent
state: cache keys are `(tenant_id, workspace_id, git_head)` and
explicitly **do not** carry `agent_id`, so every agent in the
workspace — including remote A2A `plan_verifier` peers (ADR
[`0033`](0033-coding-agent-personas.md), ADR
[`0038`](0038-plan-mode-and-cross-verification.md)) — sees the same
map for the same revision.

Out of scope (deliberate v1 cuts):

- Full call-graph extraction (who calls whom across files). The
  `(file, symbol)` table this ADR produces is the substrate; a
  follow-up ADR can layer call-edges on top.
- Semantic / embedding-based search ("find files relevant to
  *implement OAuth refresh*"). The ranker uses a cheap keyword score
  in v1; an embeddings-backed ranker is a per-strategy follow-up.
- Doc-comment extraction beyond the first leading line per symbol.
- Per-symbol cross-references (rename targets, definition-of, etc.).
  ADR [`0037`](0037-lsp-diagnostics.md)'s LSP path is the right home
  for those when they land — they need server-grade semantic
  analysis.

### `RepoMap` port

```rust
// crates/ork-core/src/ports/repo_map.rs

use std::path::PathBuf;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;

use crate::ports::workspace::WorkspaceHandle;

/// One top-level symbol extracted from a source file. The shape is
/// language-agnostic — language-specific kinds collapse into the
/// `kind` enum.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Symbol {
    /// Workspace-relative path. Always forward-slash; canonicalised.
    pub path: String,
    /// Zero-based start line in `path`.
    pub start_line: u32,
    /// Zero-based end line, inclusive of the symbol's signature line.
    pub end_line: u32,
    pub kind: SymbolKind,
    /// Bare name (e.g. `WorkspaceEditor`, `parse`). Generics and
    /// receiver type are stripped.
    pub name: String,
    /// Single-line, language-shape-preserving signature
    /// (e.g. `pub fn parse(input: &str) -> Result<Plan, Error>`).
    /// The body is **never** included.
    pub signature: String,
    /// First leading doc-comment line if present, else empty.
    /// Multi-line comments are truncated at 120 chars.
    pub doc_summary: String,
    /// Producing language id (e.g. `"rust"`, `"python"`,
    /// `"typescript"`, `"go"`).
    pub language: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Struct,
    Enum,
    Trait,
    Impl,
    Function,
    Method,
    Constant,
    TypeAlias,
    Class,
    Interface,
    Variable,
}

/// Render flavour. The same `RepoMap` artefact can be re-rendered
/// without rebuilding when the caller wants a different shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderFormat {
    /// Directory tree + per-file symbol signatures, ranked and
    /// truncated to fit the token budget. Default for LLM consumption.
    Compact,
    /// Directory tree only. No symbols. Cheapest; useful for
    /// orientation when the budget is tiny.
    Tree,
    /// Flat list of symbols with paths. No tree structure. Useful for
    /// `plan_verifier` consumers that grep for a name.
    SymbolsOnly,
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Optional task hint used by the ranker. Empty string disables
    /// keyword-relevance scoring; only mtime is then used.
    pub task_hint: String,
    /// Token budget for renders produced from this map. Symbols
    /// beyond the budget are dropped, oldest-mtime / lowest-score
    /// first. The map itself stores all symbols; truncation happens
    /// at render time.
    pub token_budget: u32,
    /// Hard ceiling on files inspected. Files past this are listed in
    /// the tree but their symbols are omitted (the render marks them
    /// with a `…` placeholder). Default: 5_000.
    pub max_files: usize,
    /// Hard ceiling on file size in bytes. Files past this are listed
    /// in the tree but their symbols are omitted. Default: 1 MiB.
    pub max_file_bytes: u64,
}

/// Built artefact. Held in the cache; rendered on demand.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RepoMap {
    pub tenant_id: TenantId,
    pub workspace_id: String,
    /// `git rev-parse HEAD` at build time; empty for workspaces with
    /// no git repo.
    pub git_head: String,
    /// Workspace-relative paths in canonical lexicographic order.
    pub files: Vec<FileEntry>,
    /// All extracted symbols, in `files` order then source order.
    pub symbols: Vec<Symbol>,
    /// Time the map was built, for observability and TTL bookkeeping.
    pub built_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub language: Option<String>,
    /// File mtime as a UNIX timestamp; used by the ranker.
    pub mtime: i64,
    pub size_bytes: u64,
    /// `true` when the file exceeded `max_file_bytes` or `max_files`
    /// and so its symbols were not extracted.
    pub omitted: bool,
}

#[async_trait]
pub trait RepoMap: Send + Sync {
    /// Build (or return a cached) map of the workspace. Idempotent
    /// for the same `(tenant, workspace, git HEAD, options)` key.
    async fn build(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
        opts: BuildOptions,
    ) -> Result<RepoMap, OrkError>;

    /// Render an already-built map into a string sized for the
    /// `BuildOptions.token_budget` the map was built under (or a
    /// caller-provided override below).
    fn render(
        &self,
        map: &RepoMap,
        format: RenderFormat,
        token_budget_override: Option<u32>,
    ) -> String;

    /// Mark a single path as dirty. ADR 0029's `WorkspaceEditor` and
    /// ADR 0028's shell/git operations call this after every write
    /// or branch-changing operation; the next `build` rebuilds the
    /// affected file's symbols and bumps the cached map.
    async fn invalidate_path(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
        path: &str,
    ) -> Result<(), OrkError>;

    /// Drop the entire cached entry for the workspace. Called on
    /// branch switches / `git reset --hard`-shaped operations where
    /// per-path invalidation is more expensive than a full rebuild.
    async fn invalidate_workspace(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
    ) -> Result<(), OrkError>;
}
```

`WorkspaceHandle` is the type ADR
[`0029`](0029-workspace-file-editor.md) already defines; this ADR
does not extend it. `chrono` is already a workspace dep
(`ork-common`).

### `LocalRepoMap` (ork-integrations)

`crates/ork-integrations/src/repo_map/mod.rs` adds `LocalRepoMap`.
Its contract:

1. **Workspace-keyed lifecycle.** State is keyed by `(TenantId,
   WorkspaceHandle.id)`. Multiple `LocalAgent`s in the same
   workspace — including the nested sub-agents ADR [`0041`]
   introduces — share one entry. The shape mirrors ADR
   [`0037`](0037-lsp-diagnostics.md) deliberately so a future ADR
   can fold the two registries together if profitable.
2. **Cache.** `LocalRepoMap` takes an `Arc<dyn KeyValueCache>` from
   [`ork-cache`](../../crates/ork-cache/src/lib.rs) and stores the
   serialised `RepoMap` under
   `repo_map:v1:{tenant_id}:{workspace_id}:{git_head}`. The
   `agent_id` is **not** part of the key — the map is workspace
   state, shareable across local agents and across remote A2A
   `plan_verifier` peers that read the rendered map as their review
   context.
3. **Cache invalidation.** Three signals invalidate:
   - `WorkspaceEditor` (ADR
     [`0029`](0029-workspace-file-editor.md)) calls `invalidate_path`
     after every successful `create_file` / `update_file` /
     `delete_file` / `apply_patch`.
   - `ShellExecutor` (ADR
     [`0028`](0028-shell-executor-and-test-runners.md)) and the git
     operations port (ADR [`0030`](0030-git-operations.md)) call
     `invalidate_workspace` after any command whose post-state
     `git rev-parse HEAD` or working-tree dirty bit differs from
     pre-state.
   - The cache key carries `git_head`, so a branch switch that
     happens *outside* ork's instrumentation (operator-initiated)
     produces a cache miss the next `build` will fix. We keep both
     the explicit invalidation path and the implicit `git_head` key
     because each catches a class the other misses.
4. **Symbol extraction.** Per-language `tree-sitter` queries pinned
   per grammar version. Out-of-the-box grammars in v1:
   - `tree-sitter-rust` — `mod`, `struct`, `enum`, `trait`, `impl`,
     `fn`, `const`, `type`.
   - `tree-sitter-python` — `class`, `def`, top-level assignments.
   - `tree-sitter-typescript` (also handles JavaScript via the
     companion grammar) — `class`, `interface`, `type`, `function`,
     top-level `const` / `let`.
   - `tree-sitter-go` — `type`, `func`, `var`, `const`.
   Adding a grammar (`gopls`, `tree-sitter-java`, etc.) is a per-
   adapter follow-up; each adapter implements an internal
   `LanguageMapAdapter` trait (extensions, query strings,
   signature-shape rules).
5. **File walk.** Deterministic walk anchored at
   `WorkspaceHandle.root`, honouring `.gitignore` (via `ignore`
   crate, already a transitive dep through `code_tools`). Files past
   `max_file_bytes` and binary files (heuristic: NUL byte in first 8
   KiB) are listed in `files` with `omitted: true` but their symbols
   are skipped.
6. **Ranking and truncation.** When rendering at
   `RenderFormat::Compact` against `token_budget`:
   - Score each file `score = w_mtime * mtime_rank + w_hint *
     keyword_overlap(task_hint, path + symbol_names)`.
     `w_mtime = 1.0`, `w_hint = 2.0` when `task_hint` is non-empty,
     else `0.0`.
   - Render in score-descending order until the running token
     estimate (cheap 4-chars-per-token heuristic, same fallback ADR
     [`0032`](0032-agent-memory-and-context-compaction.md)'s
     `ProviderHintEstimator` uses) hits `token_budget`.
   - Files dropped from the symbol render still appear in the tree
     skeleton with a `…` marker so the agent knows they exist.
   - `RenderFormat::Tree` and `RenderFormat::SymbolsOnly` apply the
     same budget but to their own shape.
7. **Audit.** Each `build` emits a `tracing` event `repo_map.build`
   with `tenant_id`, `workspace_id`, `git_head`, `files_scanned`,
   `symbols_extracted`, `cache_hit: bool`, `duration_ms`. Each
   `render` emits `repo_map.render` with `format`,
   `tokens_estimated`, `symbols_rendered`,
   `symbols_dropped_to_budget`. Both feed ADR
   [`0022`](0022-observability.md)'s audit stream.

Worked example — a tiny Rust crate rendered at
`RenderFormat::Compact` against a 400-token budget:

```text
crates/ork-cache/
├── Cargo.toml
└── src/
    └── lib.rs
        pub trait KeyValueCache: Send + Sync { … }
          fn get(&self, key: &str) -> Result<Option<Vec<u8>>, OrkError>
          fn set_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), OrkError>
          fn delete(&self, key: &str) -> Result<(), OrkError>
        pub struct RedisCache { … }
          pub async fn connect(url: &str) -> Result<Self, OrkError>
          pub fn from_connection_manager(conn: ConnectionManager) -> Self
        pub struct InMemoryCache { … }
          pub fn new() -> Self
```

`RenderFormat::Tree` of the same workspace omits the indented
signatures; `RenderFormat::SymbolsOnly` drops the indentation and
emits `crates/ork-cache/src/lib.rs:KeyValueCache (trait)`.

### Native tool: `get_repo_map`

`get_repo_map` registers through
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs)
so it surfaces in ADR
[`0011`](0011-native-llm-tool-calling.md)'s
`tool_descriptors_for_agent`.

```json
{
  "name": "get_repo_map",
  "description": "Return a structural index of the active workspace: a directory tree plus top-level symbol signatures (functions, types, traits) per file. Useful for orientation before reading or editing files. Bodies are never included; use read_file to fetch a symbol's body.",
  "parameters": {
    "type": "object",
    "properties": {
      "format": {
        "type": "string",
        "enum": ["compact", "tree", "symbols-only"],
        "default": "compact"
      },
      "task_hint": {
        "type": "string",
        "description": "Optional natural-language hint about the current task. Used to rank files: hits in path or symbol names float to the top of the rendered budget."
      },
      "token_budget": {
        "type": "integer",
        "minimum": 256,
        "maximum": 32000,
        "description": "Maximum tokens this rendering should consume. Defaults to the active model profile's repo_map_token_budget."
      }
    }
  }
}
```

Result wire shape:

```json
{
  "format": "compact",
  "rendered": "crates/\n├── ork-cache/\n│   └── src/lib.rs\n│       pub trait KeyValueCache { … }\n…",
  "git_head": "b9f25b5",
  "files_total": 412,
  "files_rendered": 87,
  "symbols_total": 2178,
  "symbols_rendered": 412,
  "tokens_estimated": 3920,
  "truncated": true,
  "cache_hit": true
}
```

`truncated` is `true` when the budget clipped the result;
`cache_hit` lets observability separate "cold build under user gaze"
from "served from cache." The `rendered` string is what the LLM
consumes; the counts are for telemetry and for the `plan_verifier`'s
verdict reasoning.

### Auto-priming at task start

ADR [`0034`](0034-per-model-capability-profiles.md)'s
`ModelCapabilityProfile` gains a single field:

```rust
pub struct ModelCapabilityProfile {
    // … existing fields …
    /// When set, the agent loop seeds the first iteration with a
    /// synthetic `get_repo_map` tool result rendered at this token
    /// budget. `0` disables auto-priming. Default: `4096` for
    /// weak-tier profiles; `0` for frontier-tier profiles.
    pub repo_map_token_budget: u32,
}
```

When non-zero, the
[`LocalAgent`](../../crates/ork-agents/src/local.rs) loop's first
iteration prepends a synthetic tool-result message of shape
`get_repo_map(format = compact, task_hint = <task description>,
token_budget = profile.repo_map_token_budget)` — same shape the LLM
would have got had it called the tool itself, so the rest of the
loop is unchanged.

This is the same auto-injection pattern ADR
[`0037`](0037-lsp-diagnostics.md) uses for `get_diagnostics` after
writes, and the same pre-emption ADR
[`0038`](0038-plan-mode-and-cross-verification.md) relies on for
plan-mode priming. The flag is overridable per persona (ADR
[`0033`](0033-coding-agent-personas.md)) — the `plan_verifier`
persona pins it on regardless of profile because verification is
useless without the structural ground truth.

### Plan-verifier consumption

ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
persona's default `tool_catalog` includes `get_repo_map`. ADR
[`0038`](0038-plan-mode-and-cross-verification.md)'s plan-
verification gate calls it explicitly when dispatching the plan to a
remote A2A peer: the rendered map travels as a `DataPart` alongside
the plan so the verifier sees the same structural snapshot the
planner saw. Because the cache key omits `agent_id`, a verifier on a
different model — possibly a different tenant's hosted frontier
model — gets the same bytes the planner did, which is what makes
"this plan references symbol `X` but the map shows `X` was deleted"
mechanical instead of speculative.

### Configuration

`config/default.toml` gains a `[repo_map]` section:

```toml
[repo_map]
enabled = true
default_token_budget = 4096
default_max_files = 5000
default_max_file_bytes = 1048576    # 1 MiB
cache_ttl_seconds = 86400           # 24 h; HEAD-keyed so churn is fine
default_min_severity = "info"

# Ranking weights for the compact renderer.
weight_mtime = 1.0
weight_task_hint = 2.0

# Per-language enable list. Disabling a language drops its
# tree-sitter grammar from the symbol pass; the files still appear
# in the tree.
languages = ["rust", "python", "typescript", "go"]
```

Operators may disable the whole feature (`enabled = false`) — the
tool then returns an error result that the LLM's loop handles like
any other tool failure — or trim the language list to control
binary size / cold-start cost.

### Boundaries with neighbouring ADRs

Stated explicitly so future ADRs do not blur them:

| Concern | Owned by |
| ------- | -------- |
| Per-task structural orientation on the working tree as it is *right now* | This ADR |
| Cross-task durable knowledge ("this repo uses tokio with `full`") | ADR [`0032`](0032-agent-memory-and-context-compaction.md) |
| Team-wide design decisions ("we chose hexagonal because …") | ADR [`0043`] (planned) |
| Semantic per-symbol queries (definition-of, references-to) | ADR [`0037`](0037-lsp-diagnostics.md) |
| Per-call-site call-graph edges | Out of scope (follow-up ADR) |
| Embedding-based file-relevance retrieval | Out of scope (follow-up ADR) |

If a future task wants any of the bottom three rows, the right
answer is a new ADR that *consumes* the symbol table this ADR
emits, not an extension here.

### RBAC scopes (reserved, not enforced)

ADR [`0021`](0021-rbac-scopes.md) owns enforcement. This ADR
reserves:

| Scope | Meaning |
| ----- | ------- |
| `tool:get_repo_map:invoke` | Agent may call `get_repo_map` |
| `repo_map:render:*` | Wildcard render permission |
| `repo_map:render:<format>` | Per-format permission (`repo_map:render:compact`, etc.) |

Enforcement lands when ADR [`0021`](0021-rbac-scopes.md)'s
`ScopeChecker` is threaded through `ToolExecutor::execute`. The
scope grammar matches that ADR's `<resource>:<id>:<action>` shape.

## Acceptance criteria

- [ ] Trait `RepoMap` defined at
      [`crates/ork-core/src/ports/repo_map.rs`](../../crates/ork-core/src/ports/repo_map.rs)
      with the signature shown in `Decision`.
- [ ] Types `Symbol`, `SymbolKind`, `RenderFormat`, `BuildOptions`,
      `RepoMap`, `FileEntry` defined in the same module and
      re-exported from
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] `LocalRepoMap` defined at
      `crates/ork-integrations/src/repo_map/mod.rs`, constructed
      from a `LocalRepoMapConfig { token_budget, max_files,
      max_file_bytes, cache_ttl, weight_mtime, weight_task_hint,
      languages }` plus an `Arc<dyn KeyValueCache>`.
- [ ] `LocalRepoMap::build` extracts top-level symbols for Rust
      sources, verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::extracts_rust_symbols`
      against a fixture under `tests/fixtures/repo_map/rust/`.
- [ ] `LocalRepoMap::build` extracts top-level symbols for Python,
      TypeScript, and Go sources, verified by sibling tests
      `extracts_python_symbols`, `extracts_typescript_symbols`,
      `extracts_go_symbols`.
- [ ] `LocalRepoMap::build` honours `.gitignore` and skips files
      past `max_file_bytes` and binary files (NUL byte heuristic),
      verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::respects_gitignore_and_size_caps`.
- [ ] Cache hits skip re-extraction when `git HEAD` is unchanged,
      verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::cache_hit_on_unchanged_head`
      against an `InMemoryCache` (asserts the second build records
      `cache_hit: true` in the emitted `tracing` span).
- [ ] Cache key is `repo_map:v1:{tenant_id}:{workspace_id}:{git_head}`
      and **does not** include `agent_id`, verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::cache_key_is_agent_agnostic`
      (two distinct `agent_id`s share the same cache entry).
- [ ] `LocalRepoMap::invalidate_path` causes the next `build` to
      re-extract that file's symbols, verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::path_invalidation_rebuilds`.
- [ ] `LocalRepoMap::invalidate_workspace` drops the entire cached
      entry, verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::workspace_invalidation_clears_cache`.
- [ ] `WorkspaceEditor` dispatcher invokes
      `RepoMap::invalidate_path` after every successful
      `create_file` / `update_file` / `delete_file` /
      `apply_patch`, verified by
      `crates/ork-integrations/tests/workspace_editor_repo_map.rs::invalidates_on_write`.
- [ ] `ShellExecutor` and the git-operations port invoke
      `RepoMap::invalidate_workspace` after any command whose
      pre/post `git rev-parse HEAD` or dirty-tree state differs,
      verified by
      `crates/ork-integrations/tests/shell_executor_repo_map.rs::invalidates_on_head_change`.
- [ ] `LocalRepoMap::render` at `RenderFormat::Compact` produces a
      string under `token_budget` and includes the directory tree
      plus signatures (no bodies), verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::compact_render_under_budget`.
- [ ] Render ranking floats files matching `task_hint` ahead of
      mtime-sorted files when budget forces truncation, verified by
      `crates/ork-integrations/tests/repo_map_smoke.rs::task_hint_ranks_files`.
- [ ] `RenderFormat::Tree` and `RenderFormat::SymbolsOnly` produce
      strings of the expected shape under the budget, verified by
      `tree_render_shape` and `symbols_only_render_shape` in the
      same test module.
- [ ] `CodeToolExecutor::is_code_tool` returns `true` for
      `"get_repo_map"`.
- [ ] `CodeToolExecutor::descriptors` returns the `get_repo_map`
      descriptor with the JSON-Schema shown under
      `Native tool: get_repo_map`.
- [ ] `CodeToolExecutor::execute` routes `get_repo_map` through
      `RepoMap::build` + `RepoMap::render` and emits the wire shape
      shown under `Native tool: get_repo_map`.
- [ ] `ModelCapabilityProfile.repo_map_token_budget: u32` field
      added in
      [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
      (or wherever ADR
      [`0034`](0034-per-model-capability-profiles.md) lands the
      profile struct) with serde round-trip coverage and defaults
      of `4096` for weak-tier profiles, `0` for frontier-tier
      profiles.
- [ ] When `repo_map_token_budget > 0`, the `LocalAgent` loop's
      first iteration prepends a synthetic `get_repo_map` tool
      result rendered at that budget, verified by
      `crates/ork-agents/tests/local_repo_map_priming.rs::prime_when_enabled`.
- [ ] When `repo_map_token_budget == 0`, no synthetic priming
      occurs, verified by
      `crates/ork-agents/tests/local_repo_map_priming.rs::no_prime_when_disabled`.
- [ ] `plan_verifier` persona's default `tool_catalog` in
      `crates/ork-agents/src/persona.rs` includes `get_repo_map`,
      and its persona-level override pins
      `repo_map_token_budget = max(profile, 4096)`.
- [ ] `cargo test -p ork-integrations repo_map::` is green.
- [ ] `cargo test -p ork-agents local_repo_map_priming::` is green.
- [ ] `[repo_map]` section added to
      [`config/default.toml`](../../config/default.toml) with the
      keys shown under `Configuration`.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row added for
      `0040`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted` / `Implemented`.

## Consequences

### Positive

- Weak local models (ADR
  [`0034`](0034-per-model-capability-profiles.md)) gain a one-shot
  orientation surface that lets a 7B model edit a medium codebase
  without 8–20 exploratory tool calls. Empirically the largest
  per-token quality win at the 4–8 K context tier (Aider, opencode,
  Cursor benchmarks).
- ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
  persona — and through it ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)'s plan-
  verification gate — gets a structural ground truth it can grep
  against, turning "the plan mentions symbol `X`" into a mechanical
  check.
- Workspace-keyed cache makes the map *free* on the second and
  subsequent calls within a task (and across tasks at the same
  HEAD), which is the common case during iterative editing. The
  agent_id-agnostic key ensures the cost is paid once per
  `(workspace, HEAD)` even when many agents share the workspace —
  including remote A2A peers.
- The shape parallels ADR [`0037`](0037-lsp-diagnostics.md)'s
  workspace-keyed pattern, which means ADR [`0041`]'s nested
  worktrees inherit the right behaviour without retrofit.
- The map is a substrate for follow-ups (call-graph,
  embedding-search, doc index) without locking those into v1's wire.

### Negative / costs

- **Cold-build latency.** A fresh build over a 600-file workspace
  is 1–4 s wall time on the v1 implementation (single-threaded
  tree-sitter parse). The first `get_repo_map` of a workspace pays
  it; every subsequent call is free until HEAD or a path
  invalidation moves. We accept this; subsequent runs amortise.
- **Cache memory.** Serialised maps are 50–500 KiB per workspace
  per HEAD on typical repos. With `cache_ttl_seconds = 86400` and
  HEAD-keyed entries a high-churn workspace can balloon Redis
  usage. Operators tune TTL downward and rely on Redis eviction;
  the map is fully reproducible from disk so a cold cache is a
  latency hit, not a correctness issue.
- **Grammar version drift.** `tree-sitter-rust` /
  `tree-sitter-python` / etc. ship breaking parser changes. We pin
  versions in `Cargo.toml` and surface adapter-init failures as
  `omitted: true` on every file in that language so the feature
  degrades to "no symbols for this language" rather than crashing.
- **Auto-priming token spend.** Profiles with
  `repo_map_token_budget = 4096` pay 4096 prompt tokens on every
  task's first iteration. For weak-tier profiles this is a strict
  win (orientation > raw budget); for the wrong tier it would be
  pure waste. Defaults err toward off; the flag's name makes the
  cost explicit in profile config.
- **Synthetic tool result in the message stream.** Auto-priming a
  `get_repo_map` result the LLM did not request mirrors ADR
  [`0037`](0037-lsp-diagnostics.md)'s `auto_diagnostics_after_edit`
  deviation from ADR [`0011`](0011-native-llm-tool-calling.md)'s
  "the LLM decides when to call tools" stance. Same trade-off, same
  guardrail (per-profile flag, default off for frontier).
- **Ranker is keyword-only in v1.** Files that match the *intent*
  of `task_hint` but share no tokens with it are deprioritised. We
  accept this for v1; embedding-backed ranking is a follow-up that
  composes on the same builder.

### Neutral / follow-ups

- Adding language grammars (Java, C#, C/C++, Ruby, Lua, Kotlin) is
  a per-adapter follow-up that does not touch the port.
- A separate ADR may add **call-graph edges** built from the same
  tree-sitter pass — the symbol table this ADR emits is the natural
  substrate.
- A separate ADR may add **embedding-backed ranking** as an
  alternative `LocalRepoMap` strategy: the trait shape is unchanged,
  only the file scoring inside `render` differs.
- A separate ADR may add **doc-comment extraction** beyond the first
  line per symbol when a richer summary is wanted (architect persona
  reading the map, for example).
- ADR [`0022`](0022-observability.md) consumes the
  `repo_map.build` / `repo_map.render` events.
- ADR [`0041`] (planned) inherits the workspace-keyed lifecycle
  free; nested sub-agents see the same map without per-agent
  duplication.
- ADR [`0043`] (planned) layers team-wide *design* decisions on top
  of the structural map this ADR provides — explicitly different
  surfaces, both useful.
- ADR [`0045`] (planned) composes diagnostics-aware (ADR
  [`0037`](0037-lsp-diagnostics.md)) and map-aware personas; the
  `plan_verifier` is the first concrete fruit.

## Alternatives considered

- **Stick with `list_tree` + `code_search` + `read_file`.**
  Rejected: this is the status quo; it costs O(8–20) tool round-
  trips before any productive edit on weak models, and gives a
  plan_verifier nothing to ground its verdict in. The existing
  tools remain in place — they answer different questions (drill
  in, grep by content, fetch a body) and compose with the map.
- **MCP "repo-indexer" community server.** Rejected for the same
  reasons ADR [`0028`](0028-shell-executor-and-test-runners.md) and
  ADR [`0037`](0037-lsp-diagnostics.md) rejected MCP for shell and
  LSP: the index is computed against a working tree owned by ADR
  [`0029`](0029-workspace-file-editor.md)'s `WorkspaceHandle`, an
  MCP server on the far side of `rmcp` does not see writes
  in time to invalidate; cross-server schema normalisation has to
  happen somewhere; native is the cheapest place. ADR
  [`0010`](0010-mcp-tool-plane.md)'s "external tools via MCP" rule
  applies to *external* surfaces — the repo map is internal.
- **ctags-style flat symbol table.** Rejected: ctags' grammar is
  per-language regexes that drift against modern syntax (Rust 2024
  edition, TypeScript decorators, Python `match` statements).
  tree-sitter's parsers are upstream-maintained and produce real
  ASTs at comparable speed. The port shape is unchanged either way;
  tree-sitter is the more durable backend.
- **LSP-driven map (reuse rust-analyzer / pyright from ADR 0037).**
  Rejected for v1: language servers are heavyweight and
  language-server-specific (rust-analyzer's `workspace/symbol`
  shape ≠ pyright's), so a unified map would still need a
  normalisation layer per server. tree-sitter gives us the same
  output across four languages with one pipeline. A future ADR can
  add an LSP-backed *enrichment* pass (definition-of, references-
  to) on the same `Symbol` shape this ADR emits.
- **Build the map per agent, not per workspace.** Rejected: ADR
  [`0033`](0033-coding-agent-personas.md)'s `plan_verifier` runs on
  a *different agent* (often a different tenant's model) than the
  planner. Per-agent caching means the verifier rebuilds from
  scratch on every plan check and may even see a slightly different
  snapshot than the planner if the tree mutated mid-check. The
  agent_id-agnostic key is the affordance that lets ADR
  [`0038`](0038-plan-mode-and-cross-verification.md) be
  mechanical instead of speculative.
- **Embedding-backed ranking in v1.** Rejected: embeddings need an
  embedding-model dependency, vector store, and per-tenant index
  management — none of which are core to the orientation problem
  this ADR solves. Keyword + mtime ranking is the 80/20; embeddings
  are a follow-up that swaps the score function without changing the
  port shape.
- **Render every call instead of caching the map.** Rejected:
  rebuilding a 600-file workspace per `get_repo_map` call (which
  the agent loop may issue several times per task at different
  `task_hint`s) would burn the cold-build cost on every
  invocation. Caching the *built artefact* and re-rendering it on
  demand is the cheap path: the build is the expensive step, the
  render is microseconds.
- **Push the map shape onto the existing `RepoWorkspace` port.**
  Rejected: `RepoWorkspace` (in
  [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs))
  is a *fetch-and-clone* port. Folding map building into it
  conflates "give me a working copy" with "give me a structural
  index of the working copy" — two lifecycles, two cache keys, two
  invalidation surfaces. Separate port keeps each concern crisp.

## Affected ork modules

- New: [`crates/ork-core/src/ports/repo_map.rs`](../../crates/ork-core/src/ports/repo_map.rs)
  — `RepoMap` port and its types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `repo_map`.
- New: `crates/ork-integrations/src/repo_map/mod.rs` —
  `LocalRepoMap`, per-workspace entry, cache integration, ranker.
- New: `crates/ork-integrations/src/repo_map/adapters/{rust.rs,
  python.rs, typescript.rs, go.rs}` — per-language tree-sitter
  adapters and the `LanguageMapAdapter` trait.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — register the `get_repo_map` tool, hold the
  `Arc<dyn RepoMap>` field.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs)
  — public re-exports for `LocalRepoMap` and the adapter registry.
- `crates/ork-integrations/src/workspace_editor.rs` (the home ADR
  [`0029`](0029-workspace-file-editor.md) lands) — invoke
  `RepoMap::invalidate_path` after every successful write.
- The shell-executor crate path (the home ADR
  [`0028`](0028-shell-executor-and-test-runners.md) lands) and the
  git-operations crate path (ADR
  [`0030`](0030-git-operations.md)) — invoke
  `RepoMap::invalidate_workspace` after HEAD- or working-tree-
  changing operations.
- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
  — extend `ModelCapabilityProfile` (per ADR
  [`0034`](0034-per-model-capability-profiles.md)) with
  `repo_map_token_budget: u32`.
- `crates/ork-agents/src/local.rs` —
  [`LocalAgent`](../../crates/ork-agents/src/local.rs) loop's
  first-iteration priming hook.
- `crates/ork-agents/src/persona.rs` (the home ADR
  [`0033`](0033-coding-agent-personas.md) lands) — `plan_verifier`
  persona default catalog includes `get_repo_map` and pins the
  budget.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
  — boot `LocalRepoMap` from `[repo_map]` config, wire it into the
  `CodeToolExecutor` builder, the `WorkspaceEditor` dispatcher, the
  `ShellExecutor`, and the git-operations port.
- [`config/default.toml`](../../config/default.toml) —
  `[repo_map]` section per `Configuration`.
- New deps in `crates/ork-integrations/Cargo.toml`:
  `tree-sitter`, `tree-sitter-rust`, `tree-sitter-python`,
  `tree-sitter-typescript`, `tree-sitter-go`, `ignore` (already a
  transitive dep, hoisted to direct).

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on
the implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3,
step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Aider | `aider/repomap.py` — directory tree + tree-sitter signatures, mtime + keyword ranking, token-budget truncation | `LocalRepoMap` + `RenderFormat::Compact` + ranker |
| opencode | `packages/opencode/src/codebase/index.ts` — workspace-keyed symbol cache exposed as a tool | `LocalRepoMap` workspace-keyed cache + `get_repo_map` tool |
| Cline | `src/services/codebase/` — file-tree + symbol summaries primed at task start | `repo_map_token_budget` auto-priming |
| Cursor (agent mode) | undocumented; observable as a structural index injected at task start | `repo_map_token_budget` auto-priming |
| Claude Code | repo-aware initial system message including a tree summary | `RenderFormat::Tree` rendering |
| Solace Agent Mesh | none — SAM has no first-class structural repo index | `RepoMap` port + `get_repo_map` tool |
| tree-sitter | language-agnostic incremental parser with maintained grammars | symbol-extraction backbone |

## Open questions

- **Multi-root workspaces.** A tenant repo containing a Rust crate
  and a Python package as siblings is handled today (each grammar's
  walker matches its own files). A repo containing two unrelated
  Cargo workspaces in subdirectories would render both into one
  map; if memory pressure forces the question, a follow-up may add
  per-sub-root maps with a join at render time.
- **Generated code.** Files under `target/`, `node_modules/`,
  `__pycache__/` are excluded by the `.gitignore` walk; build-
  artefact directories not in `.gitignore` (e.g. an
  uncommitted-but-generated `dist/`) leak into the map. We accept
  this for v1 — operators add `.gitignore` entries — but a
  per-language "generated-file heuristic" may follow.
- **Symbol-name collisions.** Two `parse` functions in different
  files render under their respective files; a flat
  `RenderFormat::SymbolsOnly` lists both (path-disambiguated). No
  collision resolution beyond the path prefix is offered.
- **Doc-comment richness.** v1 captures one leading line per symbol
  truncated at 120 chars. Some teams' code carries
  multi-paragraph rustdoc that an architect persona would benefit
  from. A `RenderFormat::CompactDocs` variant could follow.
- **Cache backend.** `KeyValueCache` is the v1 choice. If the
  serialised map grows past the size where Redis is comfortable
  (rare; the budget is bytes, not tokens), a follow-up can swap in
  a filesystem-backed entry without changing the port.
- **Cross-tenant sharing.** The cache key is keyed on `tenant_id`
  for ADR [`0020`](0020-tenant-security-and-trust.md) reasons.
  Open-source repos shared across tenants therefore rebuild
  per-tenant. A tenant-aware shared mode is conceivable but
  punts the trust analysis to ADR
  [`0020`](0020-tenant-security-and-trust.md).
- **Windows.** Acceptance criteria target Linux/macOS; Windows is
  best-effort. CI runs the smoke tests only on Linux for v1
  (matching ADR [`0037`](0037-lsp-diagnostics.md)'s posture).

## References

- ADR [`0010`](0010-mcp-tool-plane.md) — the "internal tools stay
  native" rule (and the reason `get_repo_map` is not an MCP tool).
- ADR [`0011`](0011-native-llm-tool-calling.md) — the tool-loop
  seam `get_repo_map` registers through.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) — shell
  executor that calls `invalidate_workspace` on HEAD-changing
  commands.
- ADR [`0029`](0029-workspace-file-editor.md) — `WorkspaceHandle`
  and `WorkspaceEditor` whose writes trigger `invalidate_path`.
- ADR [`0030`](0030-git-operations.md) — git operations whose
  branch / reset commands trigger `invalidate_workspace`.
- ADR [`0032`](0032-agent-memory-and-context-compaction.md) —
  durable cross-task memory the repo map deliberately is not.
- ADR [`0033`](0033-coding-agent-personas.md) — `plan_verifier`
  persona that consumes `get_repo_map` during plan review.
- ADR [`0034`](0034-per-model-capability-profiles.md) —
  `ModelCapabilityProfile` extended with `repo_map_token_budget`.
- ADR [`0037`](0037-lsp-diagnostics.md) — workspace-keyed lifecycle
  pattern this ADR mirrors.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md) — plan-
  verification gate that ships the rendered map alongside the plan.
- ADR [`0041`] (planned) — nested worktrees that inherit the
  workspace-keyed lifecycle decided here.
- ADR [`0043`] (planned) — team-wide design-decision index, the
  semantic counterpart to this structural map.
- ADR [`0045`] (planned) — multi-agent team orchestrator that
  composes map-aware personas.
- Aider repo-map: <https://aider.chat/docs/repomap.html>.
- tree-sitter: <https://tree-sitter.github.io/tree-sitter/>.
- tree-sitter-rust: <https://github.com/tree-sitter/tree-sitter-rust>.
- tree-sitter-python: <https://github.com/tree-sitter/tree-sitter-python>.
- tree-sitter-typescript:
  <https://github.com/tree-sitter/tree-sitter-typescript>.
- tree-sitter-go: <https://github.com/tree-sitter/tree-sitter-go>.
