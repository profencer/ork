//! Type equality bound for workflow builder chaining (ADR [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).

pub trait TyEq<T> {}

impl<T> TyEq<T> for T {}
