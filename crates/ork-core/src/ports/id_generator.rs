//! ID generator port stub (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).

/// Optional custom id generator for user-facing ids (messages, runs, etc.).
pub trait IdGenerator: Send + Sync {
    fn generate(&self) -> String;
}
