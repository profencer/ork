# 0020 — Tenant security and A2A trust model

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0004, 0005, 0007, 0008, 0009, 0010, 0014, 0021

**Implementation note (2026-05-08):** Phases A (RLS + enriched JWT
+ tenant CRUD scope gating), B (mesh JWT signer + cross-mesh
delegation propagation), C1 (KmsClient trait + per-tenant envelope
encryption — legacy adapter only), and D (Kafka security defaults +
Kong-headers warning) shipped under this ADR. Cloud-KMS adapters
(AWS / GCP / Azure / Vault), the `ork-push` consolidation through
`KmsClient::derive_kek_compat`, the `ork admin token mint` /
`ork admin keys rotate` CLI subcommands, the `a2a_signing_keys`
table envelope migration, and the deferred card-signing JWS path
are explicitly out of scope and are left as follow-up ADRs (see
the C1 entry under §`Reviewer findings`).

## Context

ork's tenant security has shape but several gaps:

- **RLS is configured, not enforced.** Tables in [`migrations/001_initial.sql`](../../migrations/001_initial.sql) declare `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` and policies referring to `current_setting('app.current_tenant_id')`, but no code path ever calls `SET LOCAL app.current_tenant_id = $1`. Effectively, RLS is off.
- **Tenant CRUD is public.** [`crates/ork-api/src/routes/tenants.rs`](../../crates/ork-api/src/routes/tenants.rs) is wired into `protected_routes` ([`routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs)) but the JWT in [`auth_middleware`](../../crates/ork-api/src/middleware.rs) carries an arbitrary `tenant_id`; there is no admin role check, so any authenticated tenant can list/create others.
- **A2A trust unspecified.** ADRs [`0007`](0007-remote-a2a-agent-client.md) (remote client) and [`0008`](0008-a2a-server-endpoints.md) (server) don't yet say how trust is established between meshes or between an external client and ork. Tenant id propagation across delegation chains (ADR [`0006`](0006-peer-delegation.md)) is only implicitly covered.
- **Kafka trust unspecified.** ADR [`0004`](0004-hybrid-kong-kafka-transport.md)'s topics carry sensitive data (status updates, push-notification delivery jobs); we need a default Kafka security posture.

This ADR consolidates the trust model and closes the security gaps before any of the new transports goes to production.

## Decision

ork **adopts a layered trust model** with three concentric rings: edge (Kong), mesh (ork-internal), and persistence (Postgres RLS).

### 1. Edge trust — Kong

- All HTTPS traffic enters via **Kong**. Kong terminates TLS and enforces:
  - **OAuth2** for end-user / partner traffic (DevPortal-issued tokens; standard JWT validation).
  - **mTLS** for cross-org A2A traffic. Kong validates the client cert against a configured CA and emits headers `X-Client-Cert-Subject`, `X-Client-Cert-Issuer`, `X-Client-Cert-Fingerprint`.
- Kong forwards an **ork-signed JWT** to upstream services using its [`jwt-signer`](https://docs.konghq.com/hub/kong-inc/jwt-signer/) plugin. This is the only token shape ork-api ever sees, regardless of the original client auth method. Plumbing is documented in `docs/operations/kong-routes.md` (out-of-scope for this ADR).
- Kong enforces **per-route rate limits** (replaces the in-process [`rate_limit_middleware`](../../crates/ork-api/src/middleware.rs), removed in ADR [`0008`](0008-a2a-server-endpoints.md)).

The local development setup runs without Kong; ork-api accepts a directly-signed dev JWT (the existing flow today). A startup warning is emitted when `ORK__ENV=production` and Kong-style headers are missing.

### 2. Mesh trust — JWT claims and propagation

The Kong-issued (or dev-issued) JWT carries **enriched claims** beyond today's `sub | tenant_id | exp`:

```json
{
  "sub": "user@example.com" | "client@svc",
  "tenant_id": "<uuid>",
  "tid_chain": ["<uuid>"],          // tenant chain for delegation; default: [tenant_id]
  "scopes": ["agent:planner:invoke", "tool:agent_call:invoke"],
  "trust_tier": "internal" | "partner" | "public",
  "trust_class": "user" | "service" | "agent",
  "agent_id": "vendor.scanner",     // present when trust_class == "agent"
  "exp": 1719999999,
  "iat": 1719999000,
  "iss": "kong-issuer",
  "aud": "ork-api"
}
```

[`AuthContext`](../../crates/ork-api/src/middleware.rs) is extended to surface all of these. `auth_middleware` now:

1. Parses + validates the JWT (existing behaviour).
2. Records `RequestCtx { tenant_id, user_id, scopes, trust_tier, trust_class, tid_chain }` into request extensions.
3. **Sets `app.current_tenant_id` on the connection per request transaction** by wrapping handlers in a `TenantTxScope` helper that issues `SET LOCAL app.current_tenant_id = $1` at the start of each tx. This finally activates RLS on all tenant-scoped tables.

`agent:<target>:delegate` checks (ADR [`0021`](0021-rbac-scopes.md)) read `tid_chain` to enforce cross-tenant delegation policies.

### Tenant CRUD restricted

[`crates/ork-api/src/routes/tenants.rs`](../../crates/ork-api/src/routes/tenants.rs) is gated behind a new `tenant:admin` scope. Tenant `create | list | delete` require it; tenant `read self | update self settings` require `tenant:self`. Default tokens carry `tenant:self` only; admin tokens are minted by ork's CLI (`ork admin token mint`) for operators.

### Tenant id propagation across delegation

Delegated calls (`agent_call`, `delegate_to`, ADR [`0006`](0006-peer-delegation.md)) forward the originating `tenant_id`. The `tid_chain` JWT claim carries the full chain so a remote agent receiving a delegation can audit the origin even if the immediate caller is a different tenant.

When ork's `A2aRemoteAgent` (ADR [`0007`](0007-remote-a2a-agent-client.md)) makes an outbound call, it mints a new short-lived JWT (signed with ork's mesh key) with:

- `tenant_id` = the originator's tenant id.
- `tid_chain` = the originator chain plus this hop.
- `scopes` = subset of caller's scopes that the destination card declares accepting.
- `trust_class` = `agent`, `agent_id` = ork's local caller agent id.

This is the analog of SAM's `user_identity` propagation through SAC user properties.

### 3. Kafka trust

Default posture for Kafka clusters carrying ork topics:

- **Transport**: TLS (PLAINTEXT only allowed when `ORK__ENV=dev`).
- **Auth**: SASL/OAUTHBEARER using DevPortal-issued tokens; SASL/SCRAM accepted as fallback. Kafka ACLs scope each tenant's principal to `ork.a2a.v1.*` topics by default; per-tenant topic prefixes are a future option (see ADR [`0004`](0004-hybrid-kong-kafka-transport.md) open question).
- **Discovery topics** (`discovery.agentcards`, `discovery.gatewaycards`) are readable by all internal principals; writeable only by trusted publishers (ork-api processes, plugin-registered agents).
- **Status / response topics** are tenant-data-bearing; ACLs require principal-tenant match.

Tenants do not get Kafka credentials directly; only ork processes do. External producers/consumers go through Kong-fronted A2A endpoints (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)).

### Card signing (deferred but reserved)

ADR [`0005`](0005-agent-card-and-devportal-discovery.md) leaves card signing as an open question. We **reserve** the JWS-over-card mechanism here:

- Producers sign cards with a per-process key registered in DevPortal.
- Subscribers verify against DevPortal's JWKS.
- Implemented in a follow-up ADR after this ADR is accepted; for the first cut we trust the Kafka SASL identity of the producer.

### Secrets handling

- Tenant credentials in [`TenantSettings`](../../crates/ork-core/src/models/tenant.rs) (`*_encrypted` plus the new fields from ADRs [`0010`](0010-mcp-tool-plane.md), [`0012`](0012-multi-llm-providers.md)) are AES-GCM encrypted at rest using a KEK derived from `ORK__AUTH__JWT_SECRET` (legacy) **or** from a configured KMS key when `[security.kms]` is set. We add `[security.kms]` config supporting AWS KMS, GCP KMS, Azure Key Vault, and Vault Transit.
- Per-key envelope encryption: each tenant has a randomly generated DEK; the DEK is encrypted with the KEK; both DEKs and KEKs rotate via `ork admin keys rotate`.
- Push-notification signing keys (ADR [`0009`](0009-push-notifications.md)) live in a separate `a2a_signing_keys` table with the same envelope scheme.
- Plugin-loaded code has access to whatever the operator gives it; secrets must not be passed to plugins implicitly. Plugin manifests (ADR [`0014`](0014-plugin-system.md)) declare which `secret://` references they need.

### Auditing

Every cross-trust-tier action emits a structured `tracing` event with attributes `tenant_id`, `tid_chain`, `actor`, `action`, `resource`, `result`, `request_id`. ADR [`0022`](0022-observability.md) wires these to OpenTelemetry exporters. The audit stream is not deletable from within the running process.

## Consequences

### Positive

- RLS is finally enforced; cross-tenant read/write at the SQL layer becomes impossible (subject to the operator setting `app.current_tenant_id` correctly, which we now do via middleware).
- Cross-org A2A has a real auth posture (mTLS at Kong; signed JWT in the mesh).
- Delegation chains carry an audit trail (`tid_chain`) so origin can be reconstructed.
- Tenant CRUD is no longer publicly mutable.
- KMS-backed secrets give us a credible answer for production deployments.

### Negative / costs

- Adding `SET LOCAL app.current_tenant_id = $1` per tx adds a roundtrip per tenant-scoped query path; mitigated because we already pool connections and the call is microseconds.
- Kong becomes a hard dependency in production; dev mode supports a fallback.
- KMS integration is an opt-in; without it we still ship the legacy JWT-secret-derived encryption (a documented downgrade).
- The richer JWT shape is a breaking change for existing dev tokens; we maintain backwards compatibility for one minor version with default-zero `tid_chain` and `scopes`.

### Neutral / follow-ups

- ADR [`0021`](0021-rbac-scopes.md) defines the scope vocabulary.
- ADR [`0022`](0022-observability.md) wires the audit stream.
- Card signing (deferred) gets its own ADR after JWKS infrastructure stabilises.

## Alternatives considered

- **Skip Kong; keep auth in ork-api only.** Rejected: forces ork to implement mTLS, OAuth, rate limit; duplicates the team's existing investment in Kong.
- **Per-tenant database.** Rejected: huge ops overhead; RLS is the standard pattern for SaaS multi-tenancy.
- **Trust-on-first-use for inter-mesh A2A.** Rejected: opens cross-org spoofing.
- **Enforce mTLS for all internal traffic too.** Rejected: too much operational burden for the in-cluster path; SASL/OAUTHBEARER on Kafka and Kong-issued JWT inside cover it.

## Affected ork modules

- [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs) — extend `Claims`, `AuthContext`; add `RequestCtx`; add `TenantTxScope`. Remove `rate_limit_middleware` (already addressed in ADR [`0008`](0008-a2a-server-endpoints.md)).
- [`crates/ork-api/src/routes/tenants.rs`](../../crates/ork-api/src/routes/tenants.rs) — gate by scopes.
- [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) — apply `TenantTxScope` layer.
- [`crates/ork-persistence/src/postgres/`](../../crates/ork-persistence/src/postgres/) — repositories use connections that have `app.current_tenant_id` set; helper macro `with_tenant_tx!`.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs) — envelope-encryption helpers; KMS adapter trait.
- New: `crates/ork-security/` — KMS trait + AWS / GCP / Azure / Vault adapters; mesh-token signer.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — `ork admin token mint`, `ork admin keys rotate`.
- New SQL: `migrations/007_security.sql` — tenant DEK column, signing keys table.
- [`config/default.toml`](../../config/default.toml) — `[security]` section; `[kafka]` security defaults.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| User identity propagation | SAC user properties | `tid_chain` + signed mesh JWT |
| Trust tiers | implicit in SAM gateway code | `trust_tier` claim |
| KMS-backed secrets | SAM env-driven | `crates/ork-security` KMS adapters |
| Tenant CRUD admin | SAM platform API | `tenant:admin` scope |
| Push-notification key rotation | [`common/utils/push_notification_auth.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/utils/push_notification_auth.py) | `a2a_signing_keys` + `ork admin keys rotate` |

## Open questions

- Card signing (deferred) — does the JWS sign just the card or include trust-tier metadata? Decision in a follow-up ADR.
- Should the dev-mode JWT issuer be a separate "dev-only" key path that is impossible to enable in prod by config? Yes — feature flag gated by build profile.
- Do we expose tenant-key-rotation as a self-service tenant action or admin only? Initially: admin only; tenant-self is a follow-up.

## References

- Postgres RLS: <https://www.postgresql.org/docs/current/ddl-rowsecurity.html>
- Kong jwt-signer plugin: <https://docs.konghq.com/hub/kong-inc/jwt-signer/>
- Kafka SASL/OAUTHBEARER: <https://kafka.apache.org/documentation/#security_sasl_oauthbearer>
- AWS KMS, GCP KMS, Azure Key Vault, Vault Transit

## Reviewer findings

Phase A (`feat(adr0020-a): cron tenant fix + enriched JWT/AuthContext + scope vocab` and the A3-A8 follow-up).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Critical | Migration `010_rls_policies.sql` enabled RLS + policies on `webui_projects` / `webui_conversations`, but `webui_store` does not bind `app.current_tenant_id` via `open_tenant_tx` — under a non-superuser ork-api role this would deny-all (or hard-error on cast) on every webui read. | Fixed in-session: trimmed migration 010 to only DISABLE RLS on `tenants`. The webui-table policies will land alongside the matching `webui_store` repo migration in a Phase A follow-up commit. |
| Major | `auth_middleware` propagated `tid_chain = []` verbatim from legacy JWTs, contradicting ADR §`Mesh trust — JWT claims and propagation` (canonical default `[tenant_id]`). Phase B's cross-tenant policy gate would have seen an empty chain on every legacy single-hop call. | Fixed in-session: `auth_middleware` now seeds `tenant_chain = [tenant_id]` when the JWT omits `tid_chain`. `CallerIdentity::tenant_chain` doc and the `auth_for_with_scopes` test helper updated to match. |
| Major | RLS smoke test asserted only read-side isolation; the existing `001_initial.sql` policies on `workflow_definitions` / `workflow_runs` carried `USING` only (no `WITH CHECK`), so a session under tenant A's GUC could persist a row owned by tenant B. | Fixed in-session: new `migrations/011_rls_workflow_with_check.sql` re-creates both policies with matching `WITH CHECK`; new test `cross_tenant_insert_under_a_blocked_by_with_check` asserts SQLSTATE `42501` on a forged INSERT. |
| Major | Migration 010's role contract was implicit (operators reading `migrations/` would not know "non-superuser ork-api role" is now a hard requirement). | Fixed in-session: migration 010 now opens with an explicit ROLE CONTRACT FOR THIS MIGRATION block enumerating the constraint and the per-repo migration status. |
| Minor | `set_config(..., true)` binding required `to_string()` because the function takes `text` args; future "simplify the bind" patches could regress. | Fixed in-session: comment in `tenant_scope.rs` documents the type contract. |
| Minor | `role_bypasses_rls` checked `is_superuser` only and would silently skip the assertion under a non-superuser `BYPASSRLS` role. | Fixed in-session: helper now also reads `pg_roles.rolbypassrls` for `current_user`. |
| Minor | Stale `// TODO: Run with database in testconatiners` comment (typo) in `rls_smoke.rs`. | Fixed in-session: removed. |
| Minor | `delete_tenant` allows admins to delete the tenant a token is bound to — a foot-gun, not an ADR violation. | Acknowledged, deferred to a Phase B follow-up; ADR §`Tenant CRUD restricted` mandates `tenant:admin` only and is silent on self-deletion. |
| Minor | `audit_result` collapses 401/403 to `forbidden` but tenant handlers only ever produce 403 (401 is upstream in `auth_middleware`). | Acknowledged, deferred; the helper is shared shape and narrowing is cheap but not load-bearing. |
| Nit | `tid_chain` over-indented in result-branch `tracing::info!` invocations on `tenants.rs`. | Fixed in-session: re-indented 5 occurrences. |
| Nit | `CallerIdentity::tenant_chain` doc said "Empty for top-level (single-hop) calls"; ADR canonical default is `[tenant_id]`. | Fixed in-session: doc rewritten to match the ADR. |
| Nit | Mod doc on `postgres/mod.rs` implied `tenants` was never RLS-enabled. | Fixed in-session: now states `001_initial.sql` enabled it without a policy and `migrations/010` disables it. |

Phase B (`feat(adr0020-b): mesh JWT signer + cross-mesh delegation propagation`).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Major | `execute_agent_step` synthesised `caller.scopes = vec![]`, so any LLM-driven `agent_call` / `peer_*` from a regular workflow step would now hit the new policy gate and fail with `OrkError::Validation("missing scope agent:<target>:delegate")`. | Fixed in-session: extracted `system_runtime_caller(tenant_id)` helper in `engine.rs` and applied it to both the `delegate_to:` and per-step paths. The doctrine "the engine is the system here" is now applied consistently. |
| Major | The streaming entry point (`A2aRemoteAgent::post_sse`) had no test coverage for `X-Ork-Mesh-Token`; a future refactor that drops the mint call could leave streaming traffic unattested. | Fixed in-session: new `outbound_message_stream_carries_mesh_token` test in `mesh_token_outbound.rs` drives a `wiremock` SSE response and asserts the header decodes to claims with the expected tenant + intersected scopes. |
| Minor | `mesh-trust.params.accepted_scopes` / `accepts_external_tenants` parsers silently default on type mismatch (e.g. typo'd key name), turning a misconfiguration into either deny-all or wide-open. | Acknowledged, deferred. Card-shape evolution is forward-compat by design; a follow-up boot-time validator (or a card-fetch warn-once) is the right place. |
| Minor | `MeshClaims::new` 10-argument signature carries a `#[allow(clippy::too_many_arguments)]`. | Acknowledged. Two callers; a builder would add ceremony for marginal gain. Lift to a builder when a third caller appears. |
| Minor | `HmacMeshTokenSigner.secret` field is `#[allow(dead_code)]` and the comment claiming it props up `Debug` redaction is misleading (the manual `Debug` impl is what redacts). | Acknowledged, deferred. Drop or repurpose when the RS256/JWKS migration ADR lands. |
| Minor | Mesh-token override at `auth_middleware` deliberately keeps the bearer-derived `user_id` while overwriting tenant/scopes/trust_class; rationale was undocumented at the override site. | Fixed in-session: comment added explaining the audit-trail intent (bearer.sub = immediate peer; mesh.sub = originator, captured in the `verified` audit event). |
| Minor | `extensions.rs` module-doc table omitted the new `EXT_MESH_TRUST` row. | Fixed in-session: third row added matching the existing format. |
| Nit | `auth_middleware` rejection log carries `error = %err` but no claim hints (expected_iss/aud) for ops dashboards to group on. | Acknowledged, deferred. The error message already carries the discriminant; a richer event shape is an ADR-0022 (observability) concern. |
| Nit | `child_for_delegation`'s "don't double-append target_tenant" guard is unreachable on every concrete production path. | Acknowledged. Defensive guard against a future misuse where the chain tail equals the target while `self.tenant_id` differs (e.g. a refactor that skips the outer same-tenant check). Cheap insurance. |
| Nit | `mint_mesh_token` failures abort the request before retries; rationale was undocumented. | Fixed in-session: one-line comment added at the call site explaining "signing is local CPU; a failure means misconfiguration, not transient transport". |

Phase C1 (`feat(adr0020-c1): KmsClient trait + per-tenant envelope encryption (legacy adapter, foundation)`).
Cloud-KMS adapters (AWS / GCP / Azure / Vault), the `ork-push` KEK consolidation through `KmsClient::derive_kek_compat`, and the `ork admin token mint` / `ork admin keys rotate` CLI subcommands are explicitly deferred to follow-up ADRs per the in-session decision: Phase C1 ships the foundation only.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Major | `[security.kms]` reserved variants (`aws`/`gcp`/`azure`/`vault`) silently fell back to the legacy KEK with a `tracing::warn!` only — production misconfiguration could quietly encrypt every tenant DEK under the dev-grade JWT-secret KEK. | Fixed in-session: `crates/ork-api/src/main.rs` now hard-errors at boot when a non-`Legacy` provider is configured. Forward-compat for the *config schema* (so a config file mentioning `aws` still deserialises) is preserved by the enum variants; what is NOT forward-compat is the runtime behaviour. |
| Major | `seal_field` / `try_open_field` reused `ork_push::encryption`'s AAD (`KEK_INFO`) verbatim — the same constant for every field of every tenant. A hostile-DB row swap (raw write or botched logical-replication) could move a sealed `github_token_encrypted` value into `gitlab_token_encrypted` on the same tenant and have it decrypt cleanly. | Fixed in-session: tenant-cipher now uses inline AES-GCM with `aad = "ork.tenant.field.v1\|<tenant_id>\|<field_name>"`. New tests `cross_field_swap_rejected_by_aad` and `cross_tenant_swap_rejected_by_aad` pin the property. `tenant_repo.rs` updated to pass field names through. |
| Major | `ork-security` depends on `ork-push` for crypto primitives (AES-GCM `seal/open`, `derive_kek`); ADR doc-comment claims `ork-security` is "intentionally narrow … no axum / sqlx / reqwest / rmcp / rskafka". Will become a real cycle when the deferred Phase C2+ work has `ork-push` consume `KmsClient::derive_kek_compat`. | Acknowledged, deferred. Phase C1 leaves the `ork-push`-side helpers (`derive_kek`, `seal/open` for the push signing-key envelope) in place; tenant-side encryption now uses inline AES-GCM with no `ork-push` dep. The remaining `Envelope` / `KEK_LEN` import in `tenant_cipher.rs` is for the legacy `seal_for_tenant` / `open_for_tenant` API surface that is unused by `tenant_repo` today. Lift the helpers into `ork-security` (or a new `ork-crypto` foundation crate) when the deferred ork-push consolidation lands. |
| Minor | Cache miss is a thundering-herd path under contention — N parallel readers all call `kms.unwrap` on a cold tenant. | Acknowledged, deferred. Negligible cost for the legacy adapter (HKDF-SHA256 is sub-microsecond); becomes load-bearing only when the cloud-KMS adapters land. Single-flight wrapping is a follow-up commit. |
| Minor | `decrypt_in_place` issues a second DB roundtrip per tenant fetch (`SELECT ... FROM tenants` then `SELECT dek_wrapped, ... FROM tenants`). Becomes N+1 on `list()`. | Acknowledged, deferred. The DEK columns are tiny; folding them into the main `SELECT` projection is a clean follow-up commit. Cipher cache amortises after the first call so the cold-restart cost is bounded. |
| Minor | `update_settings` re-seals every previously-sealed field with a fresh nonce on every call (since the field is decrypted by `get_by_id` and then re-encrypted before persist). Wasteful but not a security issue. | Acknowledged, deferred. Unnecessary write-amplification for the JSONB blob; tracking which fields were actually touched in the request would skip the re-seal. |
| Minor | Per-DEK seal counter is implicit (relies on `OsRng` 96-bit nonce birthday bound). No "rotate after N seals" plumbing. | Acknowledged. Bounded-N tenant fields make this comfortably safe today; the rotation ADR should include "rotate before 2^32 seals per DEK" guidance. |
| Minor | `cache_expires_after_ttl` test does not verify that `kms.unwrap` was actually called again post-TTL (no counter-mock). | Acknowledged, deferred. Add a `CountingKms` test double in a follow-up so the eviction property is positively asserted. |
| Minor | Migration column nullability silently allows perpetual half-encrypted state — pre-migration tenants that never get re-saved keep plaintext fields forever. No diagnostic surface on boot. | Acknowledged, deferred. The `ork admin keys rotate --scope tenants` subcommand (deferred to a follow-up ADR) will provide the backfill path; a boot-time log of `count(*) WHERE dek_wrapped IS NULL` is a cheap follow-up commit. |
| Minor | `KmsConfig::default()` uses `#[default]` on the `Legacy` variant; no test pins this. A future refactor that adds a new variant could silently change boot behaviour. | Fixed in-session: new `kms_config_defaults_to_legacy` test in `crates/ork-common/src/config.rs`. |
| Nit | `migrations/012_tenant_security.sql` `COMMENT ON COLUMN` strings carry trailing-backslash continuations that Postgres does not honour. | Acknowledged, deferred. Cosmetic only — `psql -c '\d+ tenants'` renders the `\` as a literal but the migration applies. |
| Nit | `lib.rs` doc says "Phase B publishes:" and lists mesh-only items — Phase C1 additions are not advertised. | Acknowledged, deferred. Doc cosmetic. |
| Nit | `tenant_cipher.rs` doc-link uses a relative filesystem path that won't resolve in rustdoc. | Acknowledged, deferred. |
| Nit | `JwtSecretKekKms::wrap` AAD (`TENANT_DEK_INFO`) is redundant given the unique HKDF context. | Acknowledged. AAD is reserved for a future `key_id` binding when cloud adapters land; harmless today. |
| Nit | `Ciphertext::CURRENT_VERSION = "v1"` is asserted via literal `"v1"` strings throughout tests. | Acknowledged, deferred. Lift assertions onto the constant when bumping the version. |

Phase D (`feat(adr0020-d): Kafka security defaults + Kong-headers warning`).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Major | `KafkaTransport::Tls` rustdoc claimed empty options ⇒ system roots; the implementation deliberately uses `RootCertStore::empty()`. Operators copying the example without `ca_path` would have hit a non-obvious "unknown issuer" handshake failure. | Fixed in-session: rustdoc on `KafkaTransport::Tls` rewritten to "**If unset**, no roots are loaded and the broker handshake will fail with 'unknown issuer'"; `config/default.toml` example annotated with the same constraint. |
| Major | `auth_middleware` read `ORK__ENV` directly while the rest of `ork-api` reads `state.config.env` (TOML + env-var override). A deployment that set `env = "production"` only in the config file would have silently skipped the Kong-headers warning. | Fixed in-session: new `RuntimeEnv(String)` newtype inserted via `Extension` from `routes::create_router_with_gateways`, sourced from `state.config.env`; middleware reads it from request extensions and falls back to `std::env::var("ORK__ENV")` only for unit-test apps that don't carry `AppState`. |
| Major | OAUTHBEARER token resolved at boot then captured in an `Arc<String>` returned verbatim from the rskafka rotation callback. DevPortal-issued tokens are short-lived; first reconnect past expiry would have failed every authenticate until restart. | Fixed in-session: callback re-resolves `std::env::var(token_env)` on every invocation so an operator-driven rotation pipeline (file-watcher / sidecar) lands without an `ork-api` restart. KMS-driven refresh is deferred to a follow-up ADR; the rustdoc now reflects the working contract. |
| Minor | Test docstring claimed a `tracing-subscriber` capture was below; none existed. | Fixed in-session: docstring rewritten to say the warn-emission assertion is deferred (the `OnceLock` makes it order-sensitive across in-process tests). |
| Minor | `KONG_HEADERS_WARNING` static is process-scoped → tests sharing the process cannot re-emit. | Acknowledged, deferred. Tests today assert only the log-and-continue half; a dedicated capture test (with `serial_test`) is a follow-up commit. |
| Minor | `tls_client_cert_without_key_is_config_error` covered only `(Some, None)`; the symmetric `(None, Some)` arm was uncovered. | Fixed in-session: added `tls_client_key_without_cert_is_config_error`. |
| Minor | Pre-ADR-0020 `security_protocol` / `sasl_mechanism` warning fired every `connect()` rather than once. | Fixed in-session: gated behind a local `OnceLock`. |
| Minor | `kafka_section_legacy_protocol_keys_still_parse` did not assert that `transport` / `auth` stayed at defaults. | Fixed in-session: added `matches!` assertions for both fields. |
| Minor | `build_client` log emitted `?std::mem::discriminant(&cfg.transport)` (opaque numeric ID) instead of variant name. | Fixed in-session: extracted `transport_kind` / `auth_kind` helpers returning `&'static str`. |
| Minor | `unsafe { std::env::set_var(...) }` blocks across the auth-middleware test suite share the process; in-parallel tests reading `ORK__ENV` could observe `"production"` mid-run. | Acknowledged, deferred. Adding `#[serial_test::serial]` is a follow-up. |
| Nit | `KafkaAuth` doc said "mirrors `[[remote_agents]]` auth shape"; field names diverge (`password_env`/`token_env` vs `value_env`). | Acknowledged, deferred. Doc cosmetic. |
| Nit | `[kafka.auth]` SCRAM example lacked context on canonical vs legacy mechanism spellings. | Fixed in-session: example annotated with the canonical / alias relationship. |
| Nit | `production_without_kong_headers_still_serves_traffic` cleans up `ORK__ENV` after the test but not on panic. | Acknowledged, deferred. A `scopeguard::defer!` (or `tempenv` test crate) is the right home for this. |
