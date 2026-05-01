# 0057 — `ork dev` / `ork build` / `ork start` CLI

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0023, 0048, 0049, 0050, 0054, 0055, 0056
- **Supersedes:** —

## Context

The current [`crates/ork-cli/`](../../crates/ork-cli/) binary
exposes a handful of subcommands tied to specific demos and
shipped features (workflow execution, peer delegation, push
notifications). What is missing is the *developer-loop CLI* a
Mastra-shaped platform expects:

- `ork dev` — spin up a local server with Studio, hot-reload on
  source changes.
- `ork build` — produce a release-mode binary that bundles the app
  + Studio + migrations.
- `ork start` — run the built artefact in production mode (Studio
  off, OTel tracing on).
- `ork init` — scaffold a new ork project with the conventional
  layout.
- `ork eval` — run the offline scorer harness from ADR 0054.
- `ork inspect` — print the `AppManifest` (ADR 0049) for a built
  binary.
- `ork lint` — sanity-check the project (id collisions,
  unreferenced workflows, schema mismatches).

Mastra's
[CLI reference](https://mastra.ai/reference/cli/mastra) ships these
exact verbs (plus `mastra studio`, `mastra migrate`, `mastra auth`,
`mastra server deploy/pause/restart/env`) for the SaaS deploy story.
We mirror the dev-loop verbs and stop at "production deployment is
the user's existing infra."

## Decision

ork **introduces the dev-loop CLI** in
[`crates/ork-cli/`](../../crates/ork-cli/) with the verbs listed
below. The CLI does not bake an opinion about how the user's app
binary is structured — it expects a Cargo workspace with a binary
crate that builds an `OrkApp` (ADR
[`0049`](0049-orkapp-central-registry.md)). The CLI orchestrates
`cargo build` / `cargo run` plus Studio bundling and dev-server
proxy.

```bash
ork init my-app                # scaffold
cd my-app

ork dev                        # cargo run + Studio + hot reload
ork build                      # cargo build --release + Studio bundle + manifest
ork start                      # run the built artefact (no Studio, OTel on)
ork inspect ./target/release/my-app   # print AppManifest
ork lint                       # static checks
ork eval --agent foo --dataset data/foo.jsonl   # offline eval
```

### Subcommand surface

```rust
// crates/ork-cli/src/main.rs (clap derive)
#[derive(Parser)]
#[command(name = "ork", version, about = "ork CLI")]
struct Cli { #[command(subcommand)] cmd: Cmd }

#[derive(Subcommand)]
enum Cmd {
    /// Boot OrkApp + REST + SSE + Studio with hot reload.
    Dev   { #[arg(long, default_value_t = 4111)] port: u16,
            #[arg(long)] no_studio: bool,
            #[arg(long)] no_open: bool,
            #[arg(long)] watch: Option<Vec<PathBuf>> },

    /// Release-build the user binary + bundle Studio assets.
    Build { #[arg(long)] release: bool },

    /// Run the built artefact in production mode.
    Start { #[arg(long)] bin: Option<PathBuf>,
            #[arg(long, default_value_t = 4111)] port: u16,
            #[arg(long, default_value_t = false)] enable_studio: bool },

    /// Scaffold a new ork project.
    Init  { name: String,
            #[arg(long, value_enum, default_value_t = Template::Minimal)]
            template: Template },

    /// Print AppManifest for a binary or a running app.
    Inspect { target: InspectTarget },

    /// Static checks against the user's project.
    Lint  { #[arg(long)] fix: bool },

    /// Offline scorer harness (ADR 0054).
    Eval  { #[arg(long)] agent: Option<String>,
            #[arg(long)] workflow: Option<String>,
            #[arg(long)] dataset: PathBuf,
            #[arg(long)] baseline: Option<PathBuf>,
            #[arg(long)] fail_on: Option<EvalFailOn>,
            #[arg(long)] output: Option<PathBuf>,
            #[arg(long, default_value_t = 4)] concurrency: usize },

    /// Apply Postgres / libsql migrations bundled with ork crates.
    Migrate { #[arg(long)] db: String,
              #[arg(long)] dry_run: bool },

    /// Run the existing demo subcommands (preserved for now).
    #[command(subcommand)]
    Legacy(LegacyCmd),
}
```

### `ork dev`

`ork dev` does the following:

1. Resolves the user's binary crate from `Cargo.toml`'s
   `[package].metadata.ork.bin` (or by a heuristic: the first
   `[[bin]]` whose dependency closure includes `ork-app`).
2. Runs `cargo build` (debug). On success, runs the resulting
   binary in a child process and waits for `/readyz` (ADR 0056).
3. Mounts Studio (ADR 0055) at `/studio` (the user's binary
   already does this if Studio is enabled in `ServerConfig`; this
   step is purely "open the browser to it").
4. Watches the user's `src/` and `workflow-templates/` directories
   (or paths from `--watch ...`). On changes:
   - Sends `SIGTERM` to the running child.
   - Re-runs `cargo build`.
   - Restarts the child.
   - Restores in-flight client connections via
     `OrkApp::reload(...)` if the rebuild is fast enough; otherwise
     surfaces a "rebuilding…" SSE event so Studio renders an
     overlay.
5. Forwards stdout/stderr from the child process to the operator's
   terminal with structured prefixing.

Hot reload is *binary restart*, not in-process patching. Rust's
type system makes "swap a function in place" infeasible without
dynamic loading; restarts within ~1–3 s are good enough and match
what `mastra dev` ships (Vite ESM HMR is faster, but Mastra's full-
graph rebuild is still in the few-second range).

### `ork build`

`ork build`:

1. `cargo build --release` on the user binary (or the workspace).
2. Builds Studio's frontend bundle (`pnpm install --frozen-lockfile
   && pnpm build` inside `crates/ork-studio/web/`) — *only* if the
   user's binary depends on `ork-studio` and the bundle hash has
   changed.
3. Produces a manifest file `target/release/ork-manifest.json`
   alongside the binary by running the binary with
   `--inspect-manifest` and dumping `AppManifest`.
4. (Optional, when packaging is requested) Tarballs the binary,
   manifest, migrations, and a small wrapper script into
   `target/release/ork-bundle.tar.gz`.

### `ork start`

`ork start` runs the release binary in production mode:

- `ServerConfig::studio = StudioConfig::Disabled` (unless
  `--enable-studio`).
- `ServerConfig::swagger_ui = false`.
- `ServerConfig::resume_on_startup = true`.
- OTel tracing exporter enabled (per ADR 0058 future).

It does **not** background the process; the operator runs it under
their existing supervisor (systemd, k8s, nomad, …). This is the
boundary where ADR
[`0023`](0023-migration-and-rollout-plan.md)'s deployment story
lives.

### `ork init`

`ork init <name>` scaffolds:

```
my-app/
├── Cargo.toml                     # workspace
├── .gitignore
├── README.md
├── src/
│   ├── main.rs                    # OrkApp::builder()...build()?.serve().await
│   └── ork/
│       ├── mod.rs
│       ├── agents/
│       │   └── weather.rs         # CodeAgent::builder("weather")...
│       ├── tools/
│       │   └── weather.rs         # tool("weather.lookup")...
│       ├── workflows/
│       │   └── weather.rs         # workflow("weather")...
│       └── scorers/               # optional
└── data/
    └── weather.jsonl              # eval dataset stub
```

Templates (`--template`):

- `minimal` — one agent, one tool, one workflow, libsql memory.
- `eval` — adds the scorers + dataset stubs.
- `multi-agent` — two agents, one calling the other.
- `mcp` — registers an external MCP server example.

### `ork inspect`

```bash
ork inspect ./target/release/my-app
ork inspect http://localhost:4111
```

The first form runs the binary with `--inspect-manifest` and prints
the manifest. The second form GETs `/api/manifest` from a running
server. Output is JSON by default; `--format table` produces a
human-readable summary.

### `ork lint`

Static checks against the user project. v1 set:

- Unique ids across categories (caught at runtime by ADR 0049, but
  surfaced earlier here).
- Workflow steps reference registered agents/tools/workflows
  (cross-reference check).
- `request_context_schema` declared on agents reachable from the
  REST surface (warn if an agent expects context but no schema is
  set).
- Eval datasets referenced by `ork eval` invocations exist on
  disk.
- Migration files are sorted and have no gaps (the existing ork
  Postgres migration discipline).

`--fix` patches simple issues (sort migrations, remove duplicate
imports). Non-trivial issues only report.

### `ork eval`

Implemented as a thin wrapper over the `OrkEval` runner from ADR
[`0054`](0054-live-scorers-and-eval-corpus.md). Boots the user's
`OrkApp` in-process (no HTTP), runs the dataset, prints a
human-readable summary plus a JSON report at `--output` (or
`./eval-report.json` by default).

### `ork migrate`

Existing migration files in
[`migrations/`](../../migrations/) are bundled into the
ork-shipping crates (memory, scorers, workflow snapshots). `ork
migrate --db postgres://...` applies pending migrations, the same
shape `sqlx migrate run` ships. `--dry-run` prints the planned
migrations without applying.

### Legacy subcommands

The current `ork-cli` subcommands (demo-specific) are gathered
under `ork legacy <subcommand>` so existing demo scripts continue
to work. They will be removed when the demos are reauthored against
the new platform shape.

## Acceptance criteria

- [ ] `crates/ork-cli/src/main.rs` exposes the subcommand tree
      shown in `Decision` via `clap` derive macros. Existing
      subcommands moved under `ork legacy`.
- [ ] `ork dev` boots the user's binary, watches `src/` for
      changes, restarts on edits, forwards stdout/stderr.
      Verified by integration test
      `crates/ork-cli/tests/dev_smoke.rs`: writes a stub project
      under a temp dir, runs `ork dev`, hits `/readyz`, edits a
      file, asserts `/readyz` is fresh after rebuild.
- [ ] `ork build` produces a release binary and (if `ork-studio`
      is in deps) a Studio bundle. The bundle hash is recorded so
      a no-source-change rebuild is a no-op. Verified by
      `crates/ork-cli/tests/build_smoke.rs`.
- [ ] `ork start` runs the built binary with production
      `ServerConfig` defaults; Studio is disabled, OTel is
      enabled, `--enable-studio` re-enables Studio. Verified.
- [ ] `ork init my-app --template minimal` creates a scaffolded
      project that `cargo build`s. The minimal template produces a
      working `weather` agent + tool + workflow.
      Verified by `crates/ork-cli/tests/init_smoke.rs`.
- [ ] `ork inspect <binary>` prints
      `AppManifest`-as-JSON; `--format table` prints a
      summary. Verified.
- [ ] `ork lint` reports id collisions, missing references,
      missing datasets; `--fix` removes one trivial class of
      problem (e.g., duplicate import). Verified.
- [ ] `ork eval` calls into `OrkEval` from ADR 0054, prints the
      report, exits non-zero on `--fail-on regression`. Verified.
- [ ] `ork migrate --db <url> --dry-run` lists pending migrations
      without applying. Apply path verified against a docker
      Postgres in CI.
- [ ] `ork dev`'s file watcher debounces (≥ 200 ms) so saving in
      bursts does not trigger a rebuild storm. Verified.
- [ ] `ork dev`'s rebuild surfaces a "rebuilding…" SSE event on
      `/api/agents/:id/stream` if the child is mid-restart;
      Studio handles this. Wired into ADR 0055.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- The dev loop is one command. `ork dev` ⇒ Studio open in
  browser ⇒ edit code ⇒ refresh. The "what command do I run?"
  cognitive cost goes to zero.
- `ork init` is the on-ramp. New users `cargo install
  ork-cli && ork init && ork dev` and have a working project in
  under a minute.
- `ork eval` is CI-ready. One bash line gates merges on scorer
  regressions (ADR 0054).
- `ork inspect` is the answer to "what does this build do?" on a
  production server when something is misbehaving.

### Negative / costs

- The CLI orchestrates `cargo` invocations from inside ork code.
  We rely on `cargo` being on PATH; documented requirement.
- File watching across platforms (macOS FSEvents, Linux inotify,
  Windows ReadDirectoryChangesW) is fiddly. Use `notify` crate;
  cover the rough edges (debounce, recursive deletes) in tests.
- Hot reload is *full restart*. A 5-second cargo build is a
  5-second pause in the dev loop. Mitigation: the watcher rebuilds
  in the background; the running binary keeps serving until the
  new one is ready.
- `ork init` templates have to evolve as the platform evolves. We
  carry one canonical template per shape and mark experimental
  templates clearly.
- `ork lint` is opinionated; users will disagree with some checks.
  Each check has a rule id and an override mechanism via
  `[package].metadata.ork.lint.disable = ["..."]`.

### Neutral / follow-ups

- `ork studio --remote https://prod.example.com` (Studio against a
  remote backend) is a Mastra parity feature; deferred until ADR
  0020 auth lands.
- `ork deploy` (push a built bundle to S3 / k8s / etc) is
  deliberately not in v1. We are a platform; deploy targets are
  the user's infrastructure.
- A future ADR can add `ork doctor` (diagnose a misbehaving local
  setup: cargo version, rust toolchain, port collisions).
- A future ADR can add `ork dataset record` (capture last N
  production runs into a JSONL dataset for ADR 0054).

## Alternatives considered

- **Skip the CLI; tell users to use `cargo run`.** Rejected.
  Mastra's success rests on the one-command dev loop. `cargo run`
  doesn't open Studio or restart on edits.
- **Use a different Rust task runner (cargo-watch, just,
  bacon).** Rejected. We invoke them under the hood (the watcher
  is `notify`, the build is `cargo build`); we just wrap them
  in an opinionated shape. Forcing users to compose `cargo-watch
  -x build -x run` plus `ork-studio serve` is the failure mode
  this ADR avoids.
- **Embed the user's app as a library that the `ork` CLI loads
  via dynamic dispatch.** Rejected. The user's `main.rs` is the
  composition root; coupling the CLI to a dynamic-loading shape
  is a Rust ergonomic loss for a marginal hot-reload win.
- **Mirror more Mastra verbs (`ork auth`, `ork server deploy`).**
  Rejected for v1. Those are Mastra Cloud / SaaS verbs; ork is
  on-prem and the deploy verb belongs to the customer's infra
  team.
- **Use a TOML config (`ork.toml`) for `dev`/`start` settings
  instead of `Cargo.toml [metadata]`.** Considered. Toml in
  `Cargo.toml` is fine; carries less cognitive load than a new
  config file. Revisit if the metadata block grows unwieldy.

## Affected ork modules

- [`crates/ork-cli/`](../../crates/ork-cli/) — major refactor:
  subcommand tree, dev orchestration, init templates, lint
  rules, inspect/eval glue.
- [`crates/ork-app/`](../../crates/) — `OrkApp::reload(new_app)`
  for hot-swap (stub from ADR 0049/0056 promoted to a real impl).
- [`crates/ork-studio/`](../../crates/ork-studio/) — `pnpm
  build` runs from ADR 0055 covered here as part of `ork build`.
- [`crates/ork-eval/`](../../crates/ork-eval/) — runner consumed
  by `ork eval`.
- [`migrations/`](../../migrations/) — `ork migrate` consumes.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [CLI reference](https://mastra.ai/reference/cli/mastra) | `ork dev/build/start/init/inspect/lint/eval/migrate` |
| Mastra | `mastra studio` | (deferred — needs auth) |
| Mastra | `mastra server deploy/pause/restart/env` | not in v1 (on-prem; user's infra) |
| Mastra | `mastra auth` | not in v1 |
| `cargo` | `cargo run` / `cargo build` / `cargo install` | invoked under the hood by `ork dev`/`ork build` |

## Open questions

- **`ork dev` resume policy.** When a rebuild fails, does the
  previous binary keep serving? Default v1: yes (fall back to
  the last good binary). Surface the build error as a Studio
  banner.
- **`ork init` template choice on prompt.** Default `minimal`;
  `--template` flag overrides. A v1.1 may prompt interactively.
- **Cross-platform watcher.** macOS, Linux, Windows. The
  `notify` crate handles all three; the test matrix in CI must
  cover at least two.
- **`ork start` graceful shutdown.** Drain timeout configured via
  `ServerConfig::shutdown_timeout`; default 30 s. Confirm
  k8s preStop semantics work.
- **Bundle size.** `ork-studio` embedded bundle is ~1 MiB
  (gzip); the ork release binary ends up ~50–80 MiB before
  strip. Acceptable for a server binary; mention in docs.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — `OrkApp` shape
  and `serve`.
- ADR [`0050`](0050-code-first-workflow-dsl.md) — workflow DSL
  the templates demonstrate.
- ADR [`0054`](0054-live-scorers-and-eval-corpus.md) — `OrkEval`
  consumed by `ork eval`.
- ADR [`0055`](0055-studio-local-dev-ui.md) — Studio bundle
  built by `ork build`.
- ADR [`0056`](0056-auto-generated-rest-and-sse-surface.md) —
  REST/SSE surface the dev server mounts.
- ADR [`0023`](0023-migration-and-rollout-plan.md) — production
  deploy story (refresh after this ADR lands).
- Mastra CLI reference:
  <https://mastra.ai/reference/cli/mastra>
