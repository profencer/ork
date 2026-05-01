//! Vector store port stub (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).
//! ADR [`0053`](../../../docs/adrs/0053-memory-working-and-semantic.md) / [`0054`](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md).

/// Optional vector index registered on `OrkApp` (crate `ork-app`).
pub trait VectorStore: Send + Sync {
    fn name(&self) -> &str;
}
