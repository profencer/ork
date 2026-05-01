//! Workflow builder — typestate `WorkflowBuilder<I, Curr, Out>`.

use std::marker::PhantomData;
use std::sync::Arc;

use ork_common::error::OrkError;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::step::Step;
use crate::Workflow;
use crate::program::ProgramOp;
use crate::trigger::TriggerSpec;
use crate::ty_eq::TyEq;

/// Start a workflow definition with the given id.
pub fn workflow(id: impl Into<String>) -> WorkflowBuilder<(), (), ()> {
    WorkflowBuilder {
        id: id.into(),
        description: String::new(),
        tool_refs: Vec::new(),
        agent_refs: Vec::new(),
        ops: Vec::new(),
        trigger: None,
        workflow_retry: None,
        workflow_timeout: None,
        _i: PhantomData,
        _c: PhantomData,
        _o: PhantomData,
    }
}

pub struct WorkflowBuilder<I, Curr, Out> {
    id: String,
    description: String,
    tool_refs: Vec<String>,
    agent_refs: Vec<String>,
    ops: Vec<ProgramOp>,
    trigger: Option<TriggerSpec>,
    workflow_retry: Option<crate::types::RetryPolicy>,
    workflow_timeout: Option<std::time::Duration>,
    _i: PhantomData<I>,
    _c: PhantomData<Curr>,
    _o: PhantomData<Out>,
}

impl<I, Curr, Out> WorkflowBuilder<I, Curr, Out> {
    pub fn description(mut self, s: impl Into<String>) -> Self {
        self.description = s.into();
        self
    }

    pub fn trigger(mut self, t: TriggerSpec) -> Self {
        self.trigger = Some(t);
        self
    }

    pub fn retry(mut self, policy: crate::types::RetryPolicy) -> Self {
        self.workflow_retry = Some(policy);
        self
    }

    pub fn timeout(mut self, d: std::time::Duration) -> Self {
        self.workflow_timeout = Some(d);
        self
    }

    pub fn input<II>(self) -> WorkflowBuilder<II, II, Out> {
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn output<OO>(self) -> WorkflowBuilder<I, Curr, OO> {
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    /// Linear composition — the previous carrier type must match this step's input.
    pub fn then<Sin, Sout>(mut self, s: Step<Sin, Sout>) -> WorkflowBuilder<I, Sout, Out>
    where
        Sin: TyEq<Curr>,
    {
        self.tool_refs.extend(s.inner.tool_refs().iter().cloned());
        self.agent_refs.extend(s.inner.agent_refs().iter().cloned());
        self.ops.push(ProgramOp::Step(s.inner));
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn map<F, X>(mut self, f: F) -> WorkflowBuilder<I, X, Out>
    where
        Curr: Serialize + DeserializeOwned,
        X: Serialize + JsonSchema,
        F: Fn(Curr) -> X + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        let op: crate::program::MapFn = Arc::new(move |v: Value| {
            let c: Curr = serde_json::from_value(v)
                .map_err(|e| OrkError::Validation(format!("map input: {e}")))?;
            let x = f(c);
            serde_json::to_value(x).map_err(|e| OrkError::Internal(format!("map output: {e}")))
        });
        self.ops.push(ProgramOp::Map(op));
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn branch(
        mut self,
        arms: Vec<(crate::types::BranchPredicate, AnyStep)>,
    ) -> WorkflowBuilder<I, Value, Out> {
        let mut branches = Vec::with_capacity(arms.len());
        for (pred, any) in arms {
            self.tool_refs.extend(any.tool_refs);
            self.agent_refs.extend(any.agent_refs);
            branches.push((pred, any.ops));
        }
        self.ops.push(ProgramOp::Branch(branches));
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn parallel(mut self, steps: Vec<AnyStep>) -> WorkflowBuilder<I, Value, Out> {
        let mut arms = Vec::with_capacity(steps.len());
        for any in steps {
            self.tool_refs.extend(any.tool_refs);
            self.agent_refs.extend(any.agent_refs);
            arms.push(any.ops);
        }
        self.ops.push(ProgramOp::Parallel(arms));
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn dountil(
        mut self,
        body: AnyStep,
        until: crate::types::Predicate,
    ) -> WorkflowBuilder<I, Value, Out> {
        self.tool_refs.extend(body.tool_refs);
        self.agent_refs.extend(body.agent_refs);
        self.ops.push(ProgramOp::DoUntil {
            body: body.ops,
            until,
        });
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn dowhile(
        mut self,
        body: AnyStep,
        while_: crate::types::Predicate,
    ) -> WorkflowBuilder<I, Value, Out> {
        self.tool_refs.extend(body.tool_refs);
        self.agent_refs.extend(body.agent_refs);
        self.ops.push(ProgramOp::DoWhile {
            body: body.ops,
            while_,
        });
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn foreach<Sin, Sout>(
        mut self,
        s: Step<Sin, Sout>,
        opts: crate::types::ForEachOptions,
    ) -> WorkflowBuilder<I, Vec<Sout>, Out>
    where
        Curr: TyEq<Vec<Sin>>,
        Sin: DeserializeOwned,
        Sout: Serialize,
    {
        self.tool_refs.extend(s.inner.tool_refs().iter().cloned());
        self.agent_refs.extend(s.inner.agent_refs().iter().cloned());
        self.ops.push(ProgramOp::ForEach {
            step: s.inner,
            opts,
        });
        WorkflowBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            ops: self.ops,
            trigger: self.trigger,
            workflow_retry: self.workflow_retry,
            workflow_timeout: self.workflow_timeout,
            _i: PhantomData,
            _c: PhantomData,
            _o: PhantomData,
        }
    }

    /// Freeze the workflow for registration on [`ork_app::OrkApp`](::ork_app::OrkApp).
    pub fn commit(self) -> Workflow
    where
        Curr: TyEq<Out>,
        I: JsonSchema,
        Out: JsonSchema,
    {
        let input_schema = serde_json::to_value(schemars::schema_for!(I)).unwrap_or(Value::Null);
        let output_schema = serde_json::to_value(schemars::schema_for!(Out)).unwrap_or(Value::Null);
        Workflow {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            program: Arc::new(self.ops),
            input_schema,
            output_schema,
            trigger: self.trigger,
        }
    }
}

/// Type-erased sub-graph (branch arm, parallel branch, loop body).
pub struct AnyStep {
    pub(crate) ops: Vec<ProgramOp>,
    pub(crate) tool_refs: Vec<String>,
    pub(crate) agent_refs: Vec<String>,
}

impl AnyStep {
    pub fn from_step<Sin, Sout>(s: Step<Sin, Sout>) -> Self {
        Self {
            tool_refs: s.inner.tool_refs().to_vec(),
            agent_refs: s.inner.agent_refs().to_vec(),
            ops: vec![ProgramOp::Step(s.inner)],
        }
    }

    pub fn empty() -> Self {
        Self {
            ops: Vec::new(),
            tool_refs: Vec::new(),
            agent_refs: Vec::new(),
        }
    }

    pub fn sequence(steps: Vec<AnyStep>) -> Self {
        let mut ops = Vec::new();
        let mut tr = Vec::new();
        let mut ar = Vec::new();
        for any in steps {
            tr.extend(any.tool_refs);
            ar.extend(any.agent_refs);
            ops.extend(any.ops);
        }
        Self {
            ops,
            tool_refs: tr,
            agent_refs: ar,
        }
    }
}
