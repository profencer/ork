//! Key-value storage port stub (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).

/// Optional KV store registered on `OrkApp` (crate `ork-app`).
pub trait KvStorage: Send + Sync {
    fn name(&self) -> &str;
}
