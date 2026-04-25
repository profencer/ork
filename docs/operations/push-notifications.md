# Operating push notifications and webhook signing

> **Audience:** operators of an `ork` deployment. Developer-facing material lives
> in [ADR-0009](../adrs/0009-push-notifications.md). This page is the runbook —
> it tells you how to bootstrap, rotate, observe and recover the signing
> infrastructure described there.

## What this subsystem does

`ork-api` lets clients register HTTPS callbacks for A2A tasks via
`tasks/pushNotificationConfig/set`. When a task hits a terminal state
(`completed`/`failed`/`canceled`/`rejected`), the JSON-RPC dispatcher publishes
an envelope onto Kafka topic `ork.a2a.v1.push.outbox`. An in-process delivery
worker consumes that topic, signs the payload with an ES256 key, and POSTs it
to every registered subscriber. Subscribers verify the signature against the
public keys served at `/.well-known/jwks.json`.

Three Postgres tables back the subsystem:

| Table | Purpose |
| ----- | ------- |
| `a2a_push_configs` | Subscriber URLs + tokens registered per task. |
| `a2a_signing_keys` | ES256 keypairs. The private PEM is sealed with AES-256-GCM. |
| `a2a_push_dead_letter` | One row per delivery that exhausted its retry budget. |

The relevant code lives in [`crates/ork-push/`](../../crates/ork-push/) (signer,
JWKS provider, delivery worker, janitor) and
[`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/) (`a2a.rs` for
the JSON-RPC handlers, `jwks.rs` for the public endpoint, `webhooks.rs` for the
inbound ack).

## Cryptography at a glance

| Component | Algorithm | Notes |
| --------- | --------- | ----- |
| Outbound signature | **ES256** (ECDSA over P-256, SHA-256) | Detached JWS in `X-A2A-Signature`; `kid` echoed in `X-A2A-Key-Id`. |
| Private key at rest | **AES-256-GCM** with a 96-bit random nonce per row | Stored in `a2a_signing_keys.private_key_pem_encrypted`/`.private_key_nonce`. |
| Key Encryption Key (KEK) | **HKDF-SHA256** of `auth.jwt_secret` | Salt `ork.a2a.push.kek.v1`; info `ork-push/signing-key-encryption`. Pure function — same secret, same KEK. |
| Public key publication | **JWK Set** at `/.well-known/jwks.json` | `Cache-Control: public, max-age=300`; both old and new keys appear during the overlap window. |

The KEK derivation lives in
[`crates/ork-push/src/encryption.rs`](../../crates/ork-push/src/encryption.rs)
and is exercised by the unit tests in the same file (round-trip, ciphertext
tamper, nonce tamper, wrong-KEK rejection). The signer uses the
[`p256`](https://crates.io/crates/p256) and
[`jsonwebtoken`](https://crates.io/crates/jsonwebtoken) crates and lives in
[`crates/ork-push/src/signing.rs`](../../crates/ork-push/src/signing.rs).

### What sits behind `ORK__AUTH__JWT_SECRET`

`ORK__AUTH__JWT_SECRET` is the **single root secret** for this subsystem. From
it `ork-push` derives:

```
KEK = HKDF-SHA256(
    salt = "ork.a2a.push.kek.v1",
    ikm  = ORK__AUTH__JWT_SECRET,
    info = "ork-push/signing-key-encryption",
    L    = 32 bytes,
)
```

That 32-byte KEK is then used to AES-256-GCM-seal each ES256 private PEM
before insert into `a2a_signing_keys`. The ciphertext is the only thing that
ever lands in the database — plaintext PEM never touches stable storage.

**Implication:** if you change `ORK__AUTH__JWT_SECRET`, every
existing row in `a2a_signing_keys` becomes undecryptable. See
[Rotating `ORK__AUTH__JWT_SECRET`](#rotating-ork__auth__jwt_secret) below for
the safe procedure.

## First-boot bootstrap

`ork-api` runs `JwksProvider::ensure_at_least_one()` during startup. If
`a2a_signing_keys` is empty it generates a fresh ES256 keypair and persists
the encrypted PEM. The first JWKS fetch will see exactly one key.

Things to watch on the very first boot:

- `ORK__AUTH__JWT_SECRET` must be set to a stable value before this point —
  re-deriving the KEK from a new secret on the next boot is fatal (see above).
- The startup log line `ADR-0009: ensured initial signing key (kid=k_…)`
  confirms the bootstrap fired.
- A second `ork-api` instance booting against the same database will **not**
  generate a duplicate key — it sees the existing row and skips.

## Configuration

All knobs live under `[push]` in `config/default.toml` and can be overridden
via `ORK__PUSH__*` environment variables. Defaults match ADR-0009 verbatim.

| Key | Default | What it controls |
| --- | ------- | ---------------- |
| `push.max_per_tenant` | `100` | Hard cap on active push configs per tenant. Enforced at `tasks/pushNotificationConfig/set`. |
| `push.request_timeout_secs` | `10` | Per-attempt HTTP timeout for the delivery worker. |
| `push.max_concurrency` | `32` | Maximum concurrent in-flight POSTs across the worker pool. |
| `push.retry_schedule_minutes` | `[1, 5, 30]` | Wait between retries. Total `n + 1` attempts (initial + len(schedule) retries). |
| `push.key_rotation_days` | `30` | Maximum age of the active signing key before `rotate_if_due` flips signers. |
| `push.key_overlap_days` | `7` | How long the rotated-out key remains in JWKS for in-flight verification. |
| `push.config_retention_days` | `14` | How long a push config row survives after the parent task hits a terminal state. The janitor reaps everything older. |

The companion `env` setting (top-level `env = "dev" \| "prod" \| …`,
`ORK__ENV`) only affects the `set` validator: in `dev` it allows
`http://localhost`, `http://127.0.0.1`, and `http://[::1]`; everywhere else
URLs **must** be `https://`.

## Background tasks the API spawns

All three honour the process-wide cancellation token, so a SIGINT / SIGTERM
shuts them down cleanly along with the HTTP server.

| Task | File | Cadence | Purpose |
| ---- | ---- | ------- | ------- |
| Delivery worker | [`crates/ork-push/src/worker.rs`](../../crates/ork-push/src/worker.rs) | Continuous (subscribes to `ork.a2a.v1.push.outbox`) | Sign + POST each envelope, retry per `push.retry_schedule_minutes`, dead-letter on exhaustion. |
| Rotation loop | [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) | Once per 24h | Calls `JwksProvider::rotate_if_due(now, force=false)`. A no-op until the active key is older than `push.key_rotation_days`. |
| Janitor | [`crates/ork-push/src/janitor.rs`](../../crates/ork-push/src/janitor.rs) | Once per hour | `delete_terminal_after(now - push.config_retention_days)` to GC `a2a_push_configs`. |

## Key rotation

### Automatic rotation

The rotation loop wakes every 24 hours and calls
`rotate_if_due(now, force=false)`. The provider compares the active key's
`created_at` against `push.key_rotation_days` (default 30) and only rotates
once that threshold is crossed. On rotation:

1. A new ES256 keypair is generated, sealed under the same KEK, and inserted
   with `expires_at = now + key_rotation_days + key_overlap_days`.
2. The previous key has `rotated_out_at = now` stamped — it stops signing new
   payloads immediately but stays in the JWKS until its `expires_at`.
3. The cached snapshot held by `JwksProvider` is refreshed in-place. The next
   `/.well-known/jwks.json` GET reflects both keys.

Subscribers caching by `kid` keep verifying any in-flight requests they
already accepted, then quietly migrate to the new `kid` on the next push.

### Manual rotation (`ork admin push rotate-keys`)

Use this when you need to invalidate a key out-of-band — e.g. a suspected
compromise or a planned crypto refresh. The CLI reads the same `AppConfig` the
API uses, opens its own Postgres pool, and calls
`rotate_if_due(now, force=true)`.

```bash
# Same env vars the API needs (ORK__DATABASE__URL, ORK__AUTH__JWT_SECRET, …):
ork admin push rotate-keys
```

The command prints a small JSON envelope on stdout:

```json
{
  "rotated": true,
  "new_kid": "k_018f7e8b…",
  "new_expires_at": "2026-06-30T10:14:22Z",
  "previous_kid": "k_018e21de…"
}
```

`previous_kid` is `null` on the very first rotation when nothing existed
before. The new key is in JWKS the moment the command returns; the API does
**not** need a restart.

> **Tip:** after a forced rotation, watch the JWKS endpoint to confirm the new
> `kid` propagated:
>
> ```bash
> curl -s https://api.example.com/.well-known/jwks.json \
>   | jq '.keys | map(.kid)'
> ```

### Rotating `ORK__AUTH__JWT_SECRET`

**This is destructive to the existing keys.** A new secret derives a new KEK,
which cannot decrypt any prior `a2a_signing_keys` rows. The migration must
re-encrypt or re-issue the keys. Pick one of two paths:

#### Path A — clean cutover (preferred)

1. Schedule a maintenance window. Subscribers caching by `kid` will see all
   existing `kid`s disappear from JWKS — they'll need to re-fetch.
2. Stop every `ork-api` replica.
3. `TRUNCATE a2a_signing_keys;` (or `DELETE FROM a2a_signing_keys;`).
4. Update `ORK__AUTH__JWT_SECRET` in your secret store.
5. Start the API. The boot path generates a fresh keypair under the new KEK.
6. Force-publish the new JWKS to subscribers via your usual notification
   channel; remind them to drop their cache.

#### Path B — re-encrypt in place (advanced)

Only viable when you still have access to the **old** secret:

1. Decrypt every row using the old KEK (`encryption::derive_kek(old_secret)`,
   then `encryption::open(...)`).
2. Re-seal with the new KEK and `UPDATE` the row.
3. Switch the secret in the secret store and roll the API.

There is no `ork admin push reseal-keys` command today — write a one-shot
script using the helpers in
[`crates/ork-push/src/encryption.rs`](../../crates/ork-push/src/encryption.rs).

### Disaster recovery: lost JWT secret

If both the secret and any backups of it are gone, the only path forward is
**Path A** above. There is no recovery for the existing private keys — that's
the entire point of envelope encryption. Subscribers will see signature
verification fail with a `kid` they no longer recognise; they should re-fetch
JWKS and continue.

## Operating the delivery worker

### What's in flight

```sql
SELECT count(*) AS active_configs
FROM   a2a_push_configs;

SELECT count(*) AS dead_today
FROM   a2a_push_dead_letter
WHERE  failed_at > now() - interval '1 day';
```

The Kafka outbox topic is `ork.a2a.v1.push.outbox` (see
[`crates/ork-eventing/src/topics.rs`](../../crates/ork-eventing/src/topics.rs)).
Lag on this topic is the canonical "the worker is keeping up" metric — your
operator dashboard from ADR-0022 should chart it.

### Tuning throughput

The two knobs that matter under load:

- `push.max_concurrency` — caps in-flight POSTs across the whole worker. Bump
  it if the consumer lag rises despite no error pattern in the dead-letter
  table.
- `push.request_timeout_secs` — slow subscribers waste a slot until they
  timeout. Lower it (e.g. 5s) if you have a noisy partner; raise it (e.g. 30s)
  if you legitimately call long-tailed integrations.

### Retry & dead-letter

Every delivery gets `1 + len(push.retry_schedule_minutes)` attempts (default
4). A non-2xx response on every attempt writes one row to
`a2a_push_dead_letter`:

```sql
SELECT failed_at, url, last_status, attempts, last_error
FROM   a2a_push_dead_letter
WHERE  tenant_id = $1
ORDER  BY failed_at DESC
LIMIT  20;
```

The `payload` column contains the verbatim envelope that would have been
delivered. There is no automatic replay — operators replay manually after the
underlying issue is fixed:

```bash
psql -tAc "SELECT payload::text FROM a2a_push_dead_letter WHERE id = '<row-id>'" \
  | curl -X POST -H 'Content-Type: application/json' \
      -H 'X-A2A-Replay: true' \
      --data-binary @- \
      https://subscriber.example.com/cb
```

A1 replay won't carry the original signature (the body is the same but the
worker re-signs at delivery time). Treat manual replays as best-effort.

### URL validation & per-tenant cap

`tasks/pushNotificationConfig/set` rejects:

- Non-`https://` URLs in any environment except `dev`. In `dev`,
  `http://localhost`, `http://127.0.0.1`, and `http://[::1]` are accepted as a
  loopback convenience for the
  [`/api/webhooks/a2a-ack`](../../crates/ork-api/src/routes/webhooks.rs)
  self-test.
- Any `set` that would push the tenant past `push.max_per_tenant`. The error
  surfaces as JSON-RPC `INVALID_PARAMS` with the message
  `push notification cap reached for tenant (… configs)`.

To inspect or evict per-tenant configs by hand:

```sql
SELECT count(*) FROM a2a_push_configs WHERE tenant_id = '<tenant-uuid>';
DELETE FROM a2a_push_configs WHERE tenant_id = '<tenant-uuid>'
  AND task_id = '<task-uuid>';
```

The janitor will eventually GC orphaned configs (`config_retention_days`),
but operators may evict early when a tenant misbehaves.

## Verifying signatures (subscriber side)

Subscribers see three custom headers on every push:

| Header | Value |
| ------ | ----- |
| `X-A2A-Signature` | Detached JWS in compact form, `<protected>..<signature>` (RFC 7515 §A.5). |
| `X-A2A-Key-Id` | The `kid` of the active signing key. Look it up in the cached JWKS. |
| `X-A2A-Timestamp` | `occurred_at` of the originating outbox envelope, RFC 3339. |
| `Authorization: Bearer …` | Only when `a2a_push_configs.token` was set; echoed verbatim. |

To verify a signature:

1. Fetch `/.well-known/jwks.json` (cache for `Cache-Control: max-age` — today
   300s).
2. Find the JWK whose `kid` matches `X-A2A-Key-Id`.
3. Reconstruct the signing input as `<X-A2A-Signature.protected> + "." +
   base64url(SHA-256(body))`.
4. Verify the third segment of `X-A2A-Signature` against that signing input
   using the JWK's `(x, y)` and ES256.

A reference implementation exists in the
[`push_outbox_delivery.rs`](../../crates/ork-api/tests/push_outbox_delivery.rs)
integration test — copy the helper into your subscriber if useful.

## Troubleshooting cheat-sheet

| Symptom | Likely cause | First check |
| ------- | ------------ | ----------- |
| `/.well-known/jwks.json` returns `{ "keys": [] }` | Boot path failed to insert the first key. | API logs for `ensure_at_least_one`; manually insert via `ork admin push rotate-keys` (force creates the first key too). |
| Subscriber sees `signature_valid=false` | Subscriber has stale JWKS cache. | Subscriber should re-fetch and respect `Cache-Control: max-age`. Confirm `X-A2A-Key-Id` matches a `kid` in JWKS. |
| Worker logs `sign push body failed: no active signing key` | KEK changed and the boot path didn't regenerate. | Check `ORK__AUTH__JWT_SECRET` history; follow [Rotating `ORK__AUTH__JWT_SECRET`](#rotating-ork__auth__jwt_secret). |
| Dead-letter rows piling up for one tenant | Subscriber genuinely down, or wrong URL. | `SELECT url, last_status, last_error FROM a2a_push_dead_letter WHERE tenant_id = …` — fix the subscriber, then replay manually. |
| `tasks/pushNotificationConfig/set` returns `cap reached` | Tenant hit `push.max_per_tenant`. | GC stale configs (see above) or raise the cap in `[push]`. |
| Outbox lag growing on `ork.a2a.v1.push.outbox` | Worker is bottlenecked. | Raise `push.max_concurrency`; lower `push.request_timeout_secs` for known-slow subscribers. |

## Backup considerations

`a2a_signing_keys` rows must be backed up alongside the rest of Postgres.
`ORK__AUTH__JWT_SECRET` must be backed up **separately** in your secrets
manager — the database backup alone is useless without the secret to derive
the KEK.

`a2a_push_dead_letter` is operational telemetry; it is safe to truncate
periodically once the dashboards have absorbed it.

## See also

- [ADR-0009 — Push notifications and webhook signing](../adrs/0009-push-notifications.md) (architectural rationale)
- [ADR-0008 — A2A server endpoints](../adrs/0008-a2a-server-endpoints.md) (where `tasks/pushNotificationConfig/{set,get}` live)
- [ADR-0022 — Observability](../adrs/0022-observability.md) (dashboards for outbox lag and dead-letter rate)
- [`crates/ork-push/`](../../crates/ork-push/) — implementation
- [`migrations/005_push_notifications.sql`](../../migrations/005_push_notifications.sql) — schema
