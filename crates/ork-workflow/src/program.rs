use std::sync::Arc;

use ork_common::error::OrkError;
use serde_json::Value;

use crate::erased::ErasedStep;
use crate::types::{BranchPredicate, ForEachOptions, Predicate};

pub(crate) type MapFn = Arc<dyn Fn(Value) -> Result<Value, OrkError> + Send + Sync>;

/// Compiled op for the ADR-0050 workflow interpreter (`ork_workflow` internal graph).
#[derive(Clone)]
pub enum ProgramOp {
    Step(Arc<dyn ErasedStep>),
    Map(MapFn),
    Branch(Vec<(BranchPredicate, Vec<ProgramOp>)>),
    Parallel(Vec<Vec<ProgramOp>>),
    DoUntil {
        body: Vec<ProgramOp>,
        until: Predicate,
    },
    DoWhile {
        body: Vec<ProgramOp>,
        while_: Predicate,
    },
    ForEach {
        step: Arc<dyn ErasedStep>,
        opts: ForEachOptions,
    },
}

impl ProgramOp {
    pub fn collect_refs(&self, tools: &mut Vec<String>, agents: &mut Vec<String>) {
        match self {
            ProgramOp::Step(s) => {
                tools.extend(s.tool_refs().iter().cloned());
                agents.extend(s.agent_refs().iter().cloned());
            }
            ProgramOp::Map(_) => {}
            ProgramOp::Branch(arms) => {
                for (_, block) in arms {
                    for op in block {
                        op.collect_refs(tools, agents);
                    }
                }
            }
            ProgramOp::Parallel(arms) => {
                for block in arms {
                    for op in block {
                        op.collect_refs(tools, agents);
                    }
                }
            }
            ProgramOp::DoUntil { body, .. } | ProgramOp::DoWhile { body, .. } => {
                for op in body {
                    op.collect_refs(tools, agents);
                }
            }
            ProgramOp::ForEach { step, .. } => {
                tools.extend(step.tool_refs().iter().cloned());
                agents.extend(step.agent_refs().iter().cloned());
            }
        }
    }
}
