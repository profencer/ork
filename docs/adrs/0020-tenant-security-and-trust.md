# 0020 — Tenant security and A2A trust model

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0004, 0005, 0007, 0008, 0009, 0010, 0014, 0021

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
