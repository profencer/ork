//! Working / semantic memory port stub (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).
//! ADR [`0053`](../../../docs/adrs/0053-memory-working-and-semantic.md) defines the real contract.

/// Optional memory backend registered on `OrkApp` (crate `ork-app`).
pub trait MemoryStore: Send + Sync {
    fn name(&self) -> &str;
}
