# 0016 — Artifact / file-management service

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0003, 0008, 0011, 0013, 0015, 0017, 0020, 0021

## Context

ork has a small "workspace" abstraction in [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs) for **read-only** access to git checkouts (search, read file, list tree). It does not let agents:

- Save outputs (a generated PDF, a CSV, a chart);
- Pass files between agents (today everything is JSON-as-string);
- Reference outputs across runs (no concept of "the report from last week's run");
- Stream large outputs to clients (gateway must hold them in memory).

A2A's `Part::File` (ADR [`0003`](0003-a2a-protocol-model.md)) and the late-phase `«artifact_content:...»` embed (ADR [`0015`](0015-dynamic-embeds.md)) both presume an artifact store exists. ADR [`0011`](0011-native-llm-tool-calling.md)'s tool-result truncation policy stores oversized results "as an artifact". ADR [`0017`](0017-webui-chat-client.md)'s Web UI needs a way to render generated files.

SAM ships an artifact service ([`agent/utils/artifact_helpers.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/artifact_helpers.py), `agent/tools/artifact_tools.py`, `services/file_management/`) with backends for local FS, S3, GCS, Azure, and Solace queue. Agents call `create_artifact`, `list_artifacts`, `load_artifact`, `artifact_meta` as standard tools.

We need the analog.

## Decision

ork **introduces an `ArtifactStore` port and a built-in artifact tool family** in `crates/ork-core` and `crates/ork-integrations`. Backend impls live in `crates/ork-storage`.

### Trait

```rust
// crates/ork-core/src/ports/artifact_store.rs

#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Storage scheme prefix this store handles (e.g. "fs", "s3", "gcs", "azblob").
    fn scheme(&self) -> &'static str;

    async fn put(&self, scope: &ArtifactScope, name: &str, body: ArtifactBody, meta: ArtifactMeta)
        -> Result<ArtifactRef, OrkError>;

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError>;
    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError>;
    async fn list(&self, scope: &ArtifactScope, prefix: Option<&str>) -> Result<Vec<ArtifactSummary>, OrkError>;
    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError>;

    /// Pre-signed download URL for clients that should fetch directly (S3 / GCS).
    /// Default impl returns None (force proxying through ork-api).
    async fn presign_get(&self, r#ref: &ArtifactRef, ttl: Duration)
        -> Result<Option<Url>, OrkError> { Ok(None) }
}

pub struct ArtifactScope {
    pub tenant_id: TenantId,
    /// Conversation/context id for cross-task artifacts. None = tenant-global.
    pub context_id: Option<ContextId>,
}

pub struct ArtifactRef {
    pub scheme: String,        // "fs" | "s3" | ...
    pub tenant_id: TenantId,
    pub context_id: Option<ContextId>,
    pub name: String,          // logical name within scope
    pub version: u32,          // monotonic; 0 = original
    pub etag: String,
}

pub struct ArtifactMeta {
    pub mime: Option<String>,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub labels: BTreeMap<String, String>,    // free-form
}

pub enum ArtifactBody {
    Bytes(Bytes),
    Stream(BoxStream<'static, Result<Bytes, OrkError>>),
}
```

### Versioning and naming

Artifacts are addressable by `(scope, name)`; each `put` with the same name creates a new version (monotonically increasing). `ArtifactRef.version = 0` aliases to "latest" for read paths; explicit versions allow auditable references. This mirrors SAM's auto-versioning behaviour.

### Built-in backends

| Scheme | Crate | Use case |
| ------ | ----- | -------- |
| `fs` | `crates/ork-storage/src/fs.rs` | Default for dev / single-node deployments; root path from config |
| `s3` | `crates/ork-storage/src/s3.rs` | AWS S3 / S3-compatible (MinIO, R2); uses `aws-sdk-s3` |
| `gcs` | `crates/ork-storage/src/gcs.rs` | Google Cloud Storage |
| `azblob` | `crates/ork-storage/src/azblob.rs` | Azure Blob Storage |

Backends behind cargo features (default: `fs`). Plugin-provided backends register through ADR [`0014`](0014-plugin-system.md).

### Chained store

A `ChainedArtifactStore` lets ork choose backend by scheme prefix in the artifact name:

```rust
pub struct ChainedArtifactStore {
    by_scheme: HashMap<String, Arc<dyn ArtifactStore>>,
    default: String,           // scheme used when name has no scheme prefix
}
```

Agents call `create_artifact("report.pdf", ...)` → goes to default; `create_artifact("s3:long-term/report.pdf", ...)` → goes to S3.

### Built-in artifact tools

Registered in [`CompositeToolExecutor`](../../crates/ork-integrations/src/tools.rs); always available subject to RBAC (`artifact:<scope>:read|write|delete`):

| Tool | Purpose | Result |
| ---- | ------- | ------ |
| `create_artifact` | Save bytes (or a base64-encoded blob in `data`) under a name | `ArtifactRef` |
| `append_artifact` | Append to an existing artifact (creates version+1 with full content) | new `ArtifactRef` |
| `list_artifacts` | List in scope, optional prefix, optional label filter | `Vec<ArtifactSummary>` |
| `load_artifact` | Read content; large reads return a presigned URL or `Part::File(uri)` | `Part::File(...)` |
| `artifact_meta` | Read metadata only | `ArtifactMeta` |
| `delete_artifact` | Hard delete a version (or all versions when `version = "*"`) | `{ deleted: N }` |
| `pin_artifact` | Add a label `pinned=true` to prevent retention sweeps | `ArtifactRef` |

These tool names match SAM's `agent/tools/artifact_tools.py` for portability of prompts.

### A2A `Part::File` integration

When ADR [`0007`](0007-remote-a2a-agent-client.md)'s remote agent receives a `Part::File { Bytes }` it streams the bytes through the configured `ArtifactStore::put`, then forwards a `Part::File { Uri }` to the inner `Agent::send`. ADR [`0008`](0008-a2a-server-endpoints.md)'s SSE bridge does the inverse on the way back if the upstream agent emits a `Part::File { Uri }` and the client requested in-band bytes.

### Embed integration

The `«artifact_content:<name> | <fmt>»` embed handler from ADR [`0015`](0015-dynamic-embeds.md) calls `ArtifactStore::get` on the resolved scope. `«artifact_meta:<name>»` calls `head`. Both live in `crates/ork-core/src/embeds/handlers/artifact.rs` and depend on `Arc<dyn ArtifactStore>` injected via `EmbedContext`.

### Gateway integration

`Gateway`s (ADR [`0013`](0013-generic-gateway-abstraction.md)) get `Arc<dyn ArtifactStore>` via `GatewayDeps` and use it to:

- Render artifact links in Slack/Teams messages (use `presign_get` if available; otherwise serve via ork-api proxy at `GET /api/artifacts/{ref}`).
- Save uploaded files (e.g. user drops a CSV in the Web UI) before passing them to the agent as `Part::File { Uri }`.

The proxy route `GET /api/artifacts/{ref}` is added to [`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/) with auth + RBAC. URL format: `GET /api/artifacts/{tenant_id}/{context_id?}/{name}/{version?}`.

### Scope and tenant isolation

`ArtifactScope.tenant_id` is **mandatory**. Backends partition data physically by tenant prefix:

- `fs`: `<root>/<tenant_id>/<context_id?>/<name>/v<version>`
- `s3`/`gcs`/`azblob`: bucket per cluster; key prefix `<tenant_id>/<context_id?>/<name>/<version>`

For `fs`, the root is set per-process; for cloud backends, bucket and key prefix are config. Cross-tenant access is rejected at the trait layer.

### Retention

A retention sweep job runs daily:

- Artifacts with `pinned=true` label: never deleted.
- Artifacts older than `retention.default_days` (default 30): hard delete.
- Artifacts attached to terminated tasks older than `retention.task_artifacts_days` (default 90): hard delete.
- Tenant policy override via `TenantSettings.artifact_retention_days`.

Implemented as a background tokio task in `ork-api` (or a dedicated worker for big deployments).

### Persistence (metadata index)

A metadata index lives in Postgres so `list_artifacts` is fast without listing object stores:

```sql
CREATE TABLE artifacts (
    tenant_id   UUID NOT NULL REFERENCES tenants(id),
    context_id  UUID,
    name        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    scheme      TEXT NOT NULL,
    storage_key TEXT NOT NULL,                  -- backend-internal key
    mime        TEXT,
    size        BIGINT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by  TEXT,
    task_id     UUID REFERENCES a2a_tasks(id),
    labels      JSONB NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (tenant_id, name, version, COALESCE(context_id, '00000000-0000-0000-0000-000000000000'::uuid))
);

CREATE INDEX artifacts_context_idx ON artifacts(context_id);
CREATE INDEX artifacts_task_id_idx ON artifacts(task_id);
ALTER TABLE artifacts ENABLE ROW LEVEL SECURITY;
CREATE POLICY artifacts_tenant_isolation ON artifacts
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);
```

`migrations/004_artifacts.sql`.

## Consequences

### Positive

- Agents can produce real outputs that live beyond a single run.
- The Web UI (ADR [`0017`](0017-webui-chat-client.md)) can render artifacts inline using `«artifact_content:...»`.
- Tool-result truncation (ADR [`0011`](0011-native-llm-tool-calling.md)) has a sensible spillover destination.
- Per-tenant isolation by physical path is auditable and easy to back up / migrate.

### Negative / costs

- Adds another datastore dependency (object store) for non-trivial deployments. Mitigated by `fs` default for dev and small installs.
- Retention sweep is a real ops job; we document the dashboards in ADR [`0022`](0022-observability.md).
- Versioning adds storage cost; the retention policy compensates but operators must tune it.

### Neutral / follow-ups

- A future ADR may add **content-addressed storage** (CAS) so identical artifacts deduplicate across tenants — only useful if storage cost becomes painful.
- Encryption-at-rest for the `fs` backend is out of scope here; cloud backends inherit provider encryption.
- `RepoWorkspace` ([`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)) is **not** absorbed by `ArtifactStore`; they are different abstractions (read-only git checkout vs. read/write blob store).

## Alternatives considered

- **Use the database for blob storage.** Rejected: Postgres is the wrong tool for multi-MB binary blobs, and existing tenant DB sizing assumptions break.
- **Skip the metadata index; list directly from the backend.** Rejected: cross-backend listing is slow and inconsistent; the Postgres index solves it cheaply.
- **One backend only (S3).** Rejected: forces every deployment to provision S3, which is a non-starter for dev and small-team installs.
- **Reuse `RepoWorkspace`.** Rejected: read-only, git-shaped, not for opaque blobs.

## Affected ork modules

- New port: `crates/ork-core/src/ports/artifact_store.rs`.
- New crate: `crates/ork-storage/` — `fs.rs`, `s3.rs`, `gcs.rs`, `azblob.rs`, `chained.rs`.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) — `artifact_*` tool arms in `CompositeToolExecutor`.
- New: `crates/ork-api/src/routes/artifacts.rs` — `GET /api/artifacts/{...}` proxy endpoint.
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) — `artifact_store: Arc<dyn ArtifactStore>`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — boot store from `[artifacts]` config.
- New SQL: `migrations/004_artifacts.sql`.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs) — `TenantSettings.artifact_retention_days: Option<u32>`.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Artifact helpers | [`agent/utils/artifact_helpers.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/artifact_helpers.py) | `crates/ork-core/src/ports/artifact_store.rs` |
| Artifact tools (`create_artifact`, `list_artifacts`, …) | [`agent/tools/artifact_tools.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/artifact_tools.py) | `artifact_*` tool family |
| File management backends | [`services/file_management/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/services/file_management) | `crates/ork-storage/src/{fs,s3,gcs,azblob}.rs` |
| Versioning | SAM auto-version on save | Same in `put` |
| Pre-signed URLs | SAM cloud backends | `presign_get` |

## Open questions

- Stream uploads from the Web UI (chunked) for very large files? Decision: yes when `s3`/`gcs` backend is used (multipart upload); fallback for `fs`.
- MIME-sniff vs trust-client header? Decision: trust client; sniffer can be added as a post-processing step in retention sweeps.
- Per-context vs per-tenant artifact garbage collection? Both — context completes → mark eligible after `task_artifacts_days`.

## References

- A2A `FilePart` spec: <https://github.com/google/a2a>
- SAM artifact tools: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/artifact_tools.py>
- AWS SDK for Rust: <https://github.com/awslabs/aws-sdk-rust>
