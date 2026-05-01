# 0042 — Capability-tagged agent discovery for coding teams

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0005, 0006, 0007, 0020, 0027, 0033, 0034, 0038, 0045
- **Supersedes:** —

## Context

ADR [`0005`](0005-agent-card-and-devportal-discovery.md) gave ork a
working *registry*: every local agent publishes an `AgentCard`, every
ork-api process subscribes to the
`ork.a2a.v1.discovery.agentcards` Kafka topic, and
[`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs) exposes
`list_cards()` / `list_id_cards()` / `card_for(id)` so a peer can ask
"who is out there?". What it does **not** answer is *which one of
these agents should I dispatch this coding subtask to?*. Today, the
caller has to:

1. Iterate `AgentRegistry::list_id_cards()` from
   [`crates/ork-core/src/agent_registry.rs`](../../crates/ork-core/src/agent_registry.rs).
2. Hand-parse free-text `skills[]` (designed for DevPortal browsing, not
   machine routing) and the `coding-persona` /
   `model-profile` extensions added by ADR
   [`0033`](0033-coding-agent-personas.md) and ADR
   [`0034`](0034-per-model-capability-profiles.md).
3. Apply ad-hoc filtering at the call site.

This is exactly the lookup ADR
[`0038`](0038-plan-mode-and-cross-verification.md)'s plan-verification
gate has to do every time a workflow does not name verifiers
explicitly: "give me one peer whose role is `plan_verifier`, whose
profile's `model_id` is materially different from the planner's, and
whose `edit_format` shows it actually understands code." It is the
same lookup the upcoming team orchestrator (ADR [`0045`]) will run on
every architect / executor / reviewer / tester slot when it composes a
team. And it is the lookup any operator-facing surface (web UI from
ADR [`0017`](0017-webui-chat-client.md), DevPortal) needs to answer
"what coding agents are available right now and what can they do?".

Hard-coding agent ids into workflow templates is the wrong answer in a
multi-tenant mesh:

- The available agent set varies by tenant (ADR
  [`0020`](0020-tenant-security-and-trust.md) scopes registries by
  `TenantId`).
- The set varies over time (agents come and go on Kafka heartbeats per
  ADR [`0005`](0005-agent-card-and-devportal-discovery.md)).
- Capability tagging (role, languages, model class) is the unit
  callers actually care about — "a Rust security reviewer with > 100k
  context", "a `plan_verifier` on a model that differs from
  `gpt-4-turbo`" — not opaque agent ids.

ADR [`0033`](0033-coding-agent-personas.md) explicitly defers
"capability-based persona discovery" to this ADR: the wire surface for
persona descriptors is reserved there; the *indexing service* that
turns those descriptors into a queryable index is here. ADR
[`0034`](0034-per-model-capability-profiles.md) does the same for
model profiles: the `model-profile` extension is published, this ADR
indexes it. ADR [`0038`](0038-plan-mode-and-cross-verification.md)
explicitly names this ADR's discovery as the deterministic fallback
for picking a non-echoing verifier.

## Decision

ork **introduces** a `CapabilityDiscovery` port in `ork-core`, an
in-process `LocalCapabilityDiscovery` implementation backed by
[`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs), a new
A2A agent-card extension (`discovery-tags`) for the small set of
fields not already covered by ADR
[`0033`](0033-coding-agent-personas.md)'s `coding-persona` and ADR
[`0034`](0034-per-model-capability-profiles.md)'s `model-profile`
extensions, and a pluggable `RankingPolicy` whose
`DiversityFromSet` variant is the load-bearing primitive for ADR
[`0038`](0038-plan-mode-and-cross-verification.md)'s
cross-verification gate. Discovery is **tenant-scoped** per ADR
[`0020`](0020-tenant-security-and-trust.md); cross-tenant lending and
cross-mesh federation are explicitly out of scope.

### Capability vocabulary (`AgentCapabilities`)

The discovery service maintains an indexed projection of every
known card's relevant extensions plus a handful of fields that only
make sense at the discovery layer. The projected shape:

```rust
// crates/ork-core/src/discovery/capability.rs

/// Capability-tagged projection of an `AgentCard`. Built by
/// `CapabilityIndex::ingest` from the agent's published extensions
/// (ADR 0033 `coding-persona`, ADR 0034 `model-profile`,
/// this ADR's `discovery-tags`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentCapabilities {
    pub agent_id: AgentId,
    pub tenant_id: TenantId,

    /// From ADR 0033's `coding-persona` extension. `None` for
    /// agents that do not publish a persona (e.g. legacy gateway
    /// agents) — those still appear in the registry but are not
    /// returned by role-filtered queries.
    pub role: Option<PersonaRole>,
    pub languages: Vec<Language>,
    pub review_specialties: Vec<ReviewSpecialty>,

    /// From ADR 0034's `model-profile` extension. The discovery
    /// summary intentionally re-publishes only the fields a peer
    /// needs to decide whether to delegate to or cross-verify
    /// against this agent.
    pub model_profile_summary: Option<ModelProfileSummary>,

    /// From this ADR's `discovery-tags` extension. Tenant-set,
    /// opaque to ork; surfaced on filters and rankers but never
    /// interpreted by the platform beyond equality / ordering.
    pub cost_tier: CostTier,
    pub free_form_tags: BTreeSet<String>,

    /// Wall-clock freshness as observed by ADR 0005's TTL cache.
    /// Re-exported here so rankers can prefer recently-seen peers.
    pub last_seen: Option<SystemTime>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelProfileSummary {
    pub profile_id: ProfileId,           // ADR 0034
    pub provider_id: String,
    pub model_id: String,
    pub max_context_tokens: u32,         // copied from ModelCapabilities at index time
    pub supports_grammar_constraint: bool,
    pub supports_native_tool_calls: bool,
    pub edit_format: EditFormat,         // re-export of ADR 0034's enum
    pub thinking_mode: ThinkingMode,
}

/// Fixed enum for review specialties. Closed on purpose: the
/// orchestrator and verifier-pickers switch on these. New variants
/// are additive.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSpecialty {
    Security,
    Performance,
    ApiDesign,
    Accessibility,
    Concurrency,
    Wire,
    Testing,
    Docs,
}

/// Coarse, tenant-set cost ordering. `ork` does not interpret the
/// numeric meaning — `Cheap < Standard < Premium` is the only
/// guarantee. Operators map their billing reality onto these three
/// buckets in tenant config.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd,
         serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    Cheap,
    Standard,
    Premium,
}
```

`PersonaRole`, `Language`, `EditFormat`, `ThinkingMode`, `ProfileId`
are re-exported from ADRs
[`0033`](0033-coding-agent-personas.md) and
[`0034`](0034-per-model-capability-profiles.md). This ADR introduces
*no* new type for those.

### `discovery-tags` agent-card extension

The two pre-existing extensions cover most of the vocabulary; the
remaining fields (`review_specialties`, `cost_tier`, `free_form_tags`)
get one small new extension URI:

```
https://ork.dev/a2a/extensions/discovery-tags
```

```json
{
  "uri": "https://ork.dev/a2a/extensions/discovery-tags",
  "params": {
    "review_specialties": ["security", "performance"],
    "cost_tier": "premium",
    "free_form_tags": ["pci-dss", "rust-stable"]
  }
}
```

Rationale for not folding these into `coding-persona`: review
specialties and cost tier are operator/tenant-set tags, not persona
properties — two deployments of the same `solo_coder` persona will
have different cost tiers and different review specialties depending
on how the operator wired them up. Keeping them on a separate
extension lets `coding-persona` stay a property of the persona type
(stable, in-tree) and `discovery-tags` stay a property of the
deployment (per-instance, operator-set).

The extension is **forward-compatible**: ADR [`0045`] may add fields to
`params`; existing consumers ignore unknown keys per the A2A
extension spec. New `ReviewSpecialty` variants are an additive enum.

### `CapabilityDiscovery` port

```rust
// crates/ork-core/src/discovery/capability.rs (continued)

#[async_trait]
pub trait CapabilityDiscovery: Send + Sync {
    /// Run a single query against the index visible to `tenant`.
    /// Returns `Ok(Vec::new())` when no agent matches; the caller
    /// (typically an orchestrator) decides whether to escalate to
    /// HITL (ADR 0027), run on-self, or fail. Discovery does not
    /// itself escalate.
    async fn query(
        &self,
        tenant: &TenantId,
        query: &DiscoveryQuery,
    ) -> Result<Vec<DiscoveryHit>, OrkError>;

    /// Rebuild the projection for one agent id. Called by the
    /// `discovery-tags` ingestor when a card heartbeat changes the
    /// extension payload.
    async fn refresh(&self, agent_id: &AgentId) -> Result<(), OrkError>;
}

#[derive(Clone, Debug)]
pub struct DiscoveryQuery {
    pub filter: DiscoveryFilter,
    pub ranking: RankingPolicy,
    pub limit: u32,                       // 0 ⇒ no limit
}

/// Conjunctive predicate over `AgentCapabilities`. Every populated
/// field is required; `None` / empty means "no constraint on this
/// dimension." Filters are pure data so they can be serialised onto
/// a workflow step (ADR 0045) and round-tripped through a webhook.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryFilter {
    pub role: Option<PersonaRole>,
    pub any_language: Vec<Language>,         // disjunction inside the field
    pub all_specialties: Vec<ReviewSpecialty>, // conjunction
    pub min_context_tokens: Option<u32>,
    pub require_grammar_constraint: bool,
    pub require_native_tool_calls: bool,
    pub edit_format_in: Vec<EditFormat>,     // disjunction; empty = unconstrained
    pub max_cost_tier: Option<CostTier>,     // include this tier and below
    pub require_tags: BTreeSet<String>,      // every tag must be present
    pub exclude_agent_ids: Vec<AgentId>,     // for "not these" e.g. self
}

#[derive(Clone, Debug)]
pub enum RankingPolicy {
    /// Order by Hamming-style distance from a "wish list" template.
    /// Used by the team orchestrator to pick "the best architect we
    /// have" when several candidates pass the filter.
    ClosestMatch { wish: AgentCapabilities },

    /// Order by `cost_tier` ascending, then by `last_seen` desc as a
    /// tie-break.
    Cheapest,

    /// Order by ADR 0022's per-model latency rollup ascending; falls
    /// back to `last_seen` when no telemetry is available.
    LowestLatency,

    /// Pick agents whose capabilities are *materially different*
    /// from a given set. Load-bearing for ADR 0038's plan
    /// cross-verification: when picking N verifiers, prefer ones
    /// whose `model_profile_summary` differs from the planner's to
    /// avoid same-model echo. Distance is computed as the sum of
    /// per-field penalties documented under "Diversity scoring"
    /// below; the implementing session may tune the per-field
    /// weights but the tie-breaking order is fixed.
    DiversityFromSet { reference: Vec<AgentCapabilities> },
}

#[derive(Clone, Debug)]
pub struct DiscoveryHit {
    pub agent_id: AgentId,
    pub capabilities: AgentCapabilities,
    /// Ranker score; meaning is policy-dependent. Higher is better
    /// for `DiversityFromSet`; lower is better for `Cheapest` and
    /// `LowestLatency`. Documented per variant.
    pub score: f64,
}
```

`DiscoveryQuery::limit = 0` means "all hits." The orchestrator (ADR
[`0045`]) typically sets `limit = 1` for "the best architect" and
`limit = N` for "N diverse verifiers."

### Diversity scoring (load-bearing for ADR 0038)

`DiversityFromSet` ranks each candidate `c` by the *minimum* distance
to any member of `reference`:

```text
score(c) = min over r in reference of distance(c, r)
```

per-field penalties (sum, capped at 1.0):

| Field | Penalty when equal | Penalty when different |
| ----- | ------------------ | ---------------------- |
| `model_profile_summary.provider_id` | 0.0 | 0.40 |
| `model_profile_summary.model_id` | 0.0 | 0.30 (and 0.10 if same vendor prefix) |
| `model_profile_summary.edit_format` | 0.0 | 0.10 |
| `model_profile_summary.thinking_mode` | 0.0 | 0.10 |
| `tenant_id` | 0.0 | 0.10 |

Higher score ⇒ more different from the reference set ⇒ better
verifier candidate. When two candidates tie, fall back to `last_seen`
desc, then to lexicographic `agent_id` for determinism.

The per-field weights are guidance, not a wire contract; the
implementing session may dogfood and adjust them. The *shape*
(min-over-reference, sum-of-per-field-penalties, deterministic
tie-break) is the contract. Two candidates with identical
`model_profile_summary` always tie at zero distance, regardless of
weight tuning.

### Tenant scoping

`CapabilityDiscovery::query` takes `tenant: &TenantId` and is
implemented to:

1. Restrict the candidate set to `cap.tenant_id == tenant`.
2. Apply the filter.
3. Apply the ranker.

Cross-tenant agent lending — a tenant `B` borrowing an agent
registered under tenant `A` — is **out of scope** for this ADR. ADR
[`0020`](0020-tenant-security-and-trust.md)'s trust model covers
cross-tenant *task dispatch* under explicit grants; layering that on
top of discovery is a follow-up. Today, an agent is visible only to
its registering tenant.

The `LocalCapabilityDiscovery` implementation reads the
[`AgentRegistry`](../../crates/ork-core/src/agent_registry.rs)
maintained per ork-api process. The registry's existing TTL eviction
(ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) keeps the
index fresh; an entry whose card has been evicted disappears from
discovery on the next query.

### Fallback policy (clean contract for ADR 0045)

When `query` returns `Vec::new()` the discovery service has done its
job. The caller decides what to do next:

- **Escalate to HITL** (ADR
  [`0027`](0027-human-in-the-loop.md)) — a workflow step may declare
  `on_no_match: human` to surface the empty-result event in the web UI
  and let an operator pick by hand.
- **Run on-self** — a solo flow that asked for a `plan_verifier` and
  found none falls back to skipping the verify step (the gate
  bypass already specified in ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)).
- **Fail** — for steps where the role is mandatory (e.g. an
  `Architect` slot in a team flow), the orchestrator emits
  `OrkError::Validation("no_capable_agent")` and the run terminates.

Discovery itself does **not** retry, escalate, or fall back. It
returns the empty set; the orchestrator is the policy holder. This
keeps the trait surface small and lets ADR
[`0045`](#) treat discovery as a pure function.

### Integration points

- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  `A2aPlanCrossVerifier` calls `query` with
  `filter.role = Some(PersonaRole::PlanVerifier)`,
  `filter.exclude_agent_ids = vec![planner_agent_id]`,
  `ranking = DiversityFromSet { reference: vec![planner_capabilities] }`,
  and `limit = N` from the policy.
- ADR [`0045`] (planned team orchestrator) calls `query` once per
  slot (architect, executor, reviewer, tester, security) with the
  appropriate role / language / specialty filter and `ClosestMatch`
  ranking.
- The web UI from ADR
  [`0017`](0017-webui-chat-client.md) consumes
  `query` to render an "available agents" panel; each `DiscoveryHit`
  is shown with its capabilities and a click-to-delegate affordance.
- The DevPortal sync job from ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md) is unchanged
  (it consumes the cards directly); discovery is purely an in-mesh
  affordance for runtime routing.

### Indexing pipeline

```
Kafka discovery topic ─► AgentRegistry (ADR 0005)
                              │
                              │ (on upsert / forget / expire)
                              ▼
                        CapabilityIndex
                              │
                              │ projects card extensions to
                              │ AgentCapabilities, indexes by
                              │ (tenant_id, role, languages,
                              │ specialties, profile_summary,
                              │ cost_tier)
                              ▼
                       CapabilityDiscovery::query
```

The index is **derived state**: nothing is persisted that cannot be
reconstructed from `AgentRegistry::list_id_cards()`. On ork-api boot,
the index is built once from the seeded registry and kept in sync via
hooks on `AgentRegistry::upsert_remote`, `forget_remote`, and
`expire_stale`.

For the v1 implementation the index is a `HashMap<AgentId,
AgentCapabilities>` plus a small per-field reverse index for the
common filters (`role`, `tenant_id`); rich query optimisation is
deferred — the per-process card count today is small enough (low
hundreds at most) that linear scan is acceptable.

### Out of scope

- **Dynamic agent provisioning.** "Spin up a new agent matching this
  query" is a separate concern owned by a future ADR; this ADR
  returns the empty set when no match exists.
- **Federation across mesh boundaries.** Cross-mesh agent card
  discovery is out of scope. ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md)'s registry is
  per-mesh; this ADR inherits that boundary.
- **Cross-tenant agent lending.** Out of scope; ADR
  [`0020`](0020-tenant-security-and-trust.md) follow-up.
- **Verdict / reputation scoring.** ADR
  [`0038`](0038-plan-mode-and-cross-verification.md) flagged
  verifier-reputation-weighted ranking as a follow-up; handled
  there, not here.
- **Operator admin API for discovery.** A `list_capable_agents` MCP
  tool may surface the index to operator agents in a follow-up.
- **Persistence of the index.** Derived state; no migrations.

## Acceptance criteria

- [ ] Trait `CapabilityDiscovery` defined at
      [`crates/ork-core/src/discovery/capability.rs`](../../crates/ork-core/src/) with
      the `query` and `refresh` async methods shown in `Decision`.
- [ ] Types `AgentCapabilities`, `ModelProfileSummary`,
      `ReviewSpecialty`, `CostTier`, `DiscoveryQuery`,
      `DiscoveryFilter`, `RankingPolicy`, `DiscoveryHit` defined in
      the same module with the field shapes shown.
- [ ] `ReviewSpecialty` and `CostTier` use
      `#[serde(rename_all = "snake_case")]` and round-trip through
      `serde_json` — verified by
      `crates/ork-core/tests/discovery_serde.rs::review_specialty_and_cost_tier_roundtrip`.
- [ ] `DiscoveryFilter` derives `Default` so a filter with every
      field unconstrained is constructible via
      `DiscoveryFilter::default()`.
- [ ] Card extension serde struct `DiscoveryTagsCardExtension` defined
      at [`crates/ork-a2a/src/extensions/discovery_tags.rs`](../../crates/ork-a2a/) with
      the JSON shape in `Decision`; `crates/ork-a2a/tests/extensions_discovery_tags.rs::roundtrip`
      asserts the example payload deserialises and re-serialises byte-stable.
- [ ] URI constant
      `pub const DISCOVERY_TAGS_EXTENSION_URI: &str =
      "https://ork.dev/a2a/extensions/discovery-tags";` defined in
      the same module.
- [ ] `CapabilityIndex::ingest(&AgentCard, &TenantId, &ModelCapabilities) -> AgentCapabilities`
      builds the projection by reading
      `coding-persona`, `model-profile`, and `discovery-tags`
      extensions; absent extensions yield `None` / empty fields
      without panicking — verified by
      `crates/ork-core/tests/discovery_ingest.rs::missing_extensions_yields_partial_caps`.
- [ ] `LocalCapabilityDiscovery` implements `CapabilityDiscovery`
      against `AgentRegistry` and is wired into ork-api at
      [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
      behind an `Arc<dyn CapabilityDiscovery>` injection point.
- [ ] Tenant-scope test
      `crates/ork-core/tests/discovery_tenant.rs::query_returns_only_caller_tenant`
      registers two agents under different tenants, queries as
      tenant `A`, and asserts only `A`'s agent is returned.
- [ ] Filter test
      `crates/ork-core/tests/discovery_filter.rs::role_and_language_filter`
      registers four agents with different roles / languages and
      asserts a query for
      `role = Reviewer, any_language = [Rust]` returns exactly the
      Rust reviewer.
- [ ] Filter test
      `crates/ork-core/tests/discovery_filter.rs::cost_tier_ordering`
      registers three agents at `Cheap`, `Standard`, `Premium`, and
      asserts `max_cost_tier = Some(Standard)` returns the first two.
- [ ] Filter test
      `crates/ork-core/tests/discovery_filter.rs::min_context_tokens_filter`
      asserts `min_context_tokens = Some(100_000)` excludes a peer
      whose `model_profile_summary.max_context_tokens = 32_768`.
- [ ] Diversity ranker test
      `crates/ork-core/tests/discovery_diversity.rs::same_model_scores_zero`
      asserts a candidate with identical
      `model_profile_summary` to the reference scores `0.0`.
- [ ] Diversity ranker test
      `crates/ork-core/tests/discovery_diversity.rs::different_provider_outranks_same_provider_different_model`
      asserts that a candidate with a different `provider_id` ranks
      strictly higher than a candidate with the same `provider_id`
      but a different `model_id`.
- [ ] Diversity ranker test
      `crates/ork-core/tests/discovery_diversity.rs::min_over_reference_set`
      with `reference = [planner, prior_verifier]` asserts a
      candidate identical to *either* reference scores `0.0`.
- [ ] Diversity ranker test
      `crates/ork-core/tests/discovery_diversity.rs::tie_break_by_last_seen_then_id`
      asserts deterministic ordering when two candidates have equal
      diversity score.
- [ ] `Cheapest` ranker test
      `crates/ork-core/tests/discovery_rank_cheap.rs::orders_by_cost_tier_then_last_seen`
      verifies cost-tier ascending, last-seen desc tie-break.
- [ ] `ClosestMatch` ranker test
      `crates/ork-core/tests/discovery_rank_closest.rs::wishlist_match_wins`
      verifies a candidate matching every wish-list field outranks
      a candidate matching only some.
- [ ] Empty-result fallback test
      `crates/ork-core/tests/discovery_empty.rs::no_match_returns_empty_vec`
      asserts that a query with no matching agent returns
      `Ok(Vec::new())` (not an error) and that
      `LocalCapabilityDiscovery` does not log at `error` for this
      case.
- [ ] `limit = 0` test
      `crates/ork-core/tests/discovery_limit.rs::zero_limit_means_unbounded`
      asserts `limit = 0` returns every matching candidate; `limit
      = 3` truncates to three after ranking.
- [ ] `LocalCapabilityDiscovery` rebuilds its projection on
      `AgentRegistry::upsert_remote` / `forget_remote` /
      `expire_stale` — verified by
      `crates/ork-core/tests/discovery_index_sync.rs::card_change_propagates`
      (re-publish a card with a new `discovery-tags` payload and
      assert the next query reflects the change).
- [ ] ADR [`0038`](0038-plan-mode-and-cross-verification.md)
      integration test
      `crates/ork-core/tests/plan_gate_diversity.rs::picks_diverse_verifier_via_discovery`
      drives the plan gate with no explicit verifier names and
      asserts it picks the diversity-ranked top-1 from the
      discovery service (mocked registry with two candidates).
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0042`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s gate stops
  carrying inline same-model-detection code — it asks the discovery
  service for a diverse verifier and gets one (or `Vec::new()` and a
  documented fallback contract).
- Workflow templates and team configs (ADR [`0045`]) refer to
  *capabilities* (`role: PlanVerifier`, `any_language: [Rust]`) rather
  than agent ids, so a tenant adding or removing agents does not
  require editing every template.
- The web UI (ADR
  [`0017`](0017-webui-chat-client.md)) gets a single API for "what's
  available?" instead of re-deriving the answer from
  `list_id_cards()` plus extension-parsing in TypeScript.
- The `discovery-tags` extension is the smallest possible surface for
  the operator-set fields (`review_specialties`, `cost_tier`,
  `free_form_tags`) that do not belong on the persona type itself.
  Two extensions stay clean (one in-tree, one operator-set).
- Discovery is a pure function on the registry's current contents:
  no new persistence, no new wire shape on the data path, no new
  Kafka topic. The only new surface is the trait and the
  `discovery-tags` extension.

### Negative / costs

- The capability vocabulary is opinionated. `ReviewSpecialty` is a
  closed enum; a tenant who needs "regulatory-compliance" or
  "ml-fairness" review has to use `free_form_tags` until a future
  ADR adds the variant. Acceptable: closed enums are how ADRs
  [`0033`](0033-coding-agent-personas.md) and
  [`0034`](0034-per-model-capability-profiles.md) shape the rest of
  the persona/profile vocabulary, and `free_form_tags` is the
  pressure release valve.
- The diversity-scoring weights are guidance, not contract. Two
  legitimate implementations can disagree on which candidate wins
  when distances are close. The deterministic tie-break (last_seen
  → agent_id) bounds the harm, but operators reading dashboards
  will sometimes see "why did discovery pick *that* one?" and the
  answer is "because the weights add up that way." Mitigated by
  the dump-resolved-query helper called out in *Open questions*.
- The index lives in-process and rebuilds on every card heartbeat.
  At low hundreds of agents this is fine; past ~10K agents the
  reverse-index design will need a real query planner. Acceptable
  for v1; flagged as follow-up.
- Two parallel surfaces exist for "what does this agent do":
  the agent card's free-text `skills[]` (for DevPortal browsing)
  and this ADR's typed capability projection (for machine routing).
  Operators editing only one will produce drift. ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md) explicitly
  warns about this; the persona-extension shape is the
  authoritative source for routing.
- `discovery-tags` is a third per-deployment extension on top of
  `coding-persona` and `model-profile`. Operators have to learn
  which extension owns which field. The redacted projection on
  `model-profile` (ADR
  [`0034`](0034-per-model-capability-profiles.md)) and the persona
  vs. instance split documented above are the only mitigations;
  collapsing the extensions is rejected because it conflates
  in-tree persona shape with operator-set deployment metadata.
- Cross-tenant lending is *not* supported. A tenant operating in a
  shared mesh who wants to use a cheaper neighbour's verifier
  cannot: discovery filters on `tenant_id` first. This is a
  deliberate ADR
  [`0020`](0020-tenant-security-and-trust.md)-driven choice; the
  cost is real and the alternative (allowing it by default) would
  break tenant isolation.
- The empty-result contract pushes policy to every caller. ADR
  [`0038`](0038-plan-mode-and-cross-verification.md) and ADR
  [`0045`] both have to spell out their fallback explicitly. We
  accept this — centralising the fallback would couple discovery
  to HITL and to workflow semantics it should not know about.
- `RankingPolicy::LowestLatency` requires ADR
  [`0022`](0022-observability.md)'s per-model latency rollup to
  exist. Until that lands, the variant falls back to `last_seen`,
  which is a poor proxy. Documented in `Open questions`.

### Neutral / follow-ups

- ADR [`0038`](0038-plan-mode-and-cross-verification.md) consumes
  `DiversityFromSet` ranking and `role = PlanVerifier` filter.
- ADR [`0045`] (planned) consumes `ClosestMatch` ranking per
  team-slot and the empty-result fallback contract.
- ADR [`0020`](0020-tenant-security-and-trust.md) follow-up may
  introduce cross-tenant lending grants; this ADR's tenant-scoped
  query becomes "include lent agents" with an extra parameter.
- A future ADR may add a `list_capable_agents` MCP tool exposing
  the discovery surface to operator agents.
- A future ADR may add verifier-reputation-weighted ranking
  (per ADR [`0038`](0038-plan-mode-and-cross-verification.md)
  follow-up); the `RankingPolicy` enum is additive.
- A future ADR may extend `CostTier` with a numeric
  `cost_per_million_tokens` field driven by ADR
  [`0022`](0022-observability.md)'s per-model rollup; today's
  three-bucket enum stays as the operator-facing contract.
- A future ADR may introduce a federation layer that lets one ork
  mesh query another's discovery; the trait surface is
  intentionally narrow (`query` + `refresh`) so a federated
  implementation can wrap the local one.

## Alternatives considered

- **No discovery service — keep iterating
  `AgentRegistry::list_id_cards()` at every call site.** Rejected:
  ADR [`0038`](0038-plan-mode-and-cross-verification.md) and ADR
  [`0045`] both end up writing the same filter / rank code.
  Centralising it pays for itself the second time the diversity
  ranker is needed.
- **Fold the new fields into ADR
  [`0033`](0033-coding-agent-personas.md)'s `coding-persona`
  extension.** Rejected: persona properties are *type* properties
  (the same persona type ships in every deployment); review
  specialties and cost tier are *deployment* properties (operators
  set them per agent instance). Putting them on the persona
  extension means a tenant overriding cost tier has to "fork the
  persona," which loses the type-level invariants ADR
  [`0033`](0033-coding-agent-personas.md) shipped.
- **Treat `cost_tier` as a number from the start.** Rejected:
  any number we pick is wrong (USD per million tokens? compute
  units? GPU minutes?). The three-bucket enum is what an
  orchestrator actually needs ("at most this expensive"); the
  measured cost number is ADR [`0022`](0022-observability.md)'s
  lane.
- **Make discovery synchronous in `AgentRegistry`.** Rejected:
  `AgentRegistry` already has many methods (`list_cards`,
  `peer_tool_descriptions`, etc.); adding capability filtering /
  ranking would bloat its trait surface. The discovery service is
  a layer above; the registry stays the source of truth.
- **Use a graph database / GraphQL for the index.** Rejected:
  per-process card counts are small; reverse maps on a few common
  filters are enough. New infrastructure for v1 would be
  premature.
- **Have discovery return concrete `Arc<dyn Agent>` handles
  instead of `AgentId` + `AgentCapabilities`.** Rejected: that
  conflates discovery (a read-only routing decision) with
  resolution (an act that touches the registry's TTL cache). The
  caller already has `AgentRegistry::resolve` for the second step.
- **Separate ranking and filtering into two traits.** Rejected:
  the common case is "filter then rank in one pass" and forcing
  two trait calls would double the ceremony with no observable
  gain. The `DiscoveryQuery` struct keeps them composable for
  future implementations.
- **Allow discovery to return *partial* matches with a
  `match_quality: f32`.** Rejected: the orchestrator can encode
  graceful degradation by widening the filter on a second call;
  baking partial matches into the contract makes every consumer
  decide what "partial" means.
- **Expose discovery via a new HTTP/A2A endpoint.** Rejected:
  discovery is a routing primitive consumed by other in-process
  code (ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)'s gate, ADR
  [`0045`]'s orchestrator). An HTTP endpoint duplicates ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md)'s
  registry endpoints without adding routing semantics. A future
  operator-facing surface goes through ADR
  [`0017`](0017-webui-chat-client.md)'s gateway.
- **Hard-require the diversity ranker to enforce different
  `provider_id`.** Rejected for the same reason ADR
  [`0038`](0038-plan-mode-and-cross-verification.md)'s
  `require_distinct_verifier_model` is opt-in: a deployment with
  one provider configured still wants verification on a
  same-provider-different-model peer. The ranker *prefers*
  diversity; the *requirement* is owned by the gate's policy.
- **Index across mesh boundaries (federation) from day one.**
  Rejected: federation is its own ADR. The trait surface is
  intentionally minimal so a federated implementation can wrap the
  local one without breaking changes.

## Affected ork modules

- New: [`crates/ork-core/src/discovery/`](../../crates/ork-core/src/) —
  `capability.rs` (the trait, types, default `LocalCapabilityDiscovery`,
  `CapabilityIndex`); `mod.rs` re-exports.
- New: [`crates/ork-a2a/src/extensions/discovery_tags.rs`](../../crates/ork-a2a/src/) —
  `DiscoveryTagsCardExtension` serde struct, URI constant, matching
  unit tests at
  `crates/ork-a2a/tests/extensions_discovery_tags.rs`.
- [`crates/ork-core/src/agent_registry.rs`](../../crates/ork-core/src/agent_registry.rs)
  — small hook surface so `LocalCapabilityDiscovery` can subscribe
  to `upsert_remote` / `forget_remote` / `expire_stale` (a
  `tokio::sync::watch::Sender<u64>` epoch counter is sufficient;
  no new public method on the registry).
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
  — instantiate `LocalCapabilityDiscovery`, expose it via the
  application state for ADR [`0038`] / ADR [`0045`] consumers.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s
  `A2aPlanCrossVerifier` — call into `CapabilityDiscovery::query`
  in the no-explicit-verifier branch (replaces the inline
  same-model heuristic).
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
| Solace Agent Mesh | `discovery/>` topic + filtered subscribe | `CapabilityDiscovery` trait + `DiscoveryFilter` |
| Kubernetes | `LabelSelector` + scheduler ranking | `DiscoveryFilter` (conjunctive) + `RankingPolicy` |
| Consul | service discovery with tag-based queries | `discovery-tags` extension + `require_tags` filter |
| LangGraph | `tools.ToolNode` selection by name | rejected analogue: name-based selection is what this ADR replaces |
| Multi-agent debate (Du et al., 2023) | "ask N independent judges" | `DiversityFromSet` ranker over `PersonaRole::PlanVerifier` |
| Google Research, *Towards a Science of Scaling Agent Systems* (2025) | same-model verifiers add ~0% | weighted penalties favouring different providers / models |
| Aider | model selection by name | this ADR's `model_profile_summary` filter (`min_context_tokens`, `edit_format_in`) |

## Open questions

- **Diversity weight tuning.** The per-field penalties are seeded
  at 0.40 / 0.30 / 0.10 / 0.10 / 0.10 but we have no dogfood data
  yet. ADR [`0022`](0022-observability.md) metrics on verifier
  agreement vs. diversity score will let the implementing session
  tune. Out of scope for v1.
- **Latency-ranked policy.** `LowestLatency` depends on ADR
  [`0022`](0022-observability.md)'s per-model rollup. Until that
  lands, the variant falls back to `last_seen`. Should we instead
  refuse to construct the variant when telemetry is absent? Stance:
  no — the fallback is documented and lets workflow templates name
  the policy ahead of telemetry shipping.
- **Dump-resolved-query helper.** Operators debugging "why did
  discovery pick *that* one?" want a
  `LocalCapabilityDiscovery::explain(tenant, query) -> ExplainTrace`
  diagnostic. Tracked as a follow-up; not in v1 acceptance.
- **Free-form tag explosion.** Tenants will invent tags. The
  `require_tags` filter is conjunctive on a `BTreeSet<String>`; at
  some scale we will want a controlled vocabulary. Defer until a
  tenant requests it.
- **`AnyPhase` and `Other` handling.** `Language::Other` and
  `PersonaPhase::AnyPhase` exist in ADR
  [`0033`](0033-coding-agent-personas.md). For filters, we treat
  `Other` as never matching a specific language and `AnyPhase` as
  not relevant to discovery (phase is a runtime concept). Document
  these in the implementation; flagged here because reviewers will
  ask.
- **MCP exposure.** Adding `find_capable_agent` /
  `list_capable_agents` as native tools (per
  [`AGENTS.md`](../../AGENTS.md) §3) is a small follow-up once an
  operator-facing persona exists. Defer.
- **Federation.** Cross-mesh discovery is reserved for a future
  ADR. The trait surface (single `query` method) is shaped so a
  federated wrapper composes naturally.

## References

- A2A spec — extensions and discovery:
  <https://github.com/google/a2a>
- Du et al., *Improving Factuality and Reasoning in Language
  Models through Multiagent Debate* (2023):
  <https://arxiv.org/abs/2305.14325>
- Google Research, *Towards a Science of Scaling Agent Systems —
  When and Why Agent Systems Work* (April 2025):
  <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>
- Kubernetes label selectors:
  <https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/>
- Consul service discovery:
  <https://developer.hashicorp.com/consul/docs/discovery/services>
- Related ADRs:
  [`0005`](0005-agent-card-and-devportal-discovery.md),
  [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0020`](0020-tenant-security-and-trust.md),
  [`0027`](0027-human-in-the-loop.md),
  [`0033`](0033-coding-agent-personas.md),
  [`0034`](0034-per-model-capability-profiles.md),
  [`0038`](0038-plan-mode-and-cross-verification.md),
  0045 (forthcoming).
