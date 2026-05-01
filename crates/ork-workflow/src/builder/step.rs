//! Typed step builder and [`Step`] handle.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::erased::{ErasedStep, StepOutcome};
use crate::types::StepContext;

/// Start building a step with the given id (must match `^[a-z0-9][a-z0-9-]{0,62}$` at registration).
pub fn step(id: impl Into<String>) -> StepBuilder<(), ()> {
    StepBuilder {
        id: id.into(),
        description: String::new(),
        tool_refs: Vec::new(),
        agent_refs: Vec::new(),
        retry: None,
        timeout: None,
        _i: PhantomData,
        _o: PhantomData,
    }
}

pub struct StepBuilder<I, O> {
    id: String,
    description: String,
    tool_refs: Vec<String>,
    agent_refs: Vec<String>,
    retry: Option<crate::types::RetryPolicy>,
    timeout: Option<Duration>,
    _i: PhantomData<I>,
    _o: PhantomData<O>,
}

impl<I, O> StepBuilder<I, O> {
    pub fn input<II>(self) -> StepBuilder<II, O>
    where
        II: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    {
        StepBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            retry: self.retry,
            timeout: self.timeout,
            _i: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn output<OO>(self) -> StepBuilder<I, OO>
    where
        OO: JsonSchema + Serialize + Send + Sync + 'static,
    {
        StepBuilder {
            id: self.id,
            description: self.description,
            tool_refs: self.tool_refs,
            agent_refs: self.agent_refs,
            retry: self.retry,
            timeout: self.timeout,
            _i: PhantomData,
            _o: PhantomData,
        }
    }

    pub fn description(mut self, s: impl Into<String>) -> Self {
        self.description = s.into();
        self
    }

    pub fn retry(mut self, policy: crate::types::RetryPolicy) -> Self {
        self.retry = Some(policy);
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    pub fn uses_tool(mut self, id: impl Into<String>) -> Self {
        self.tool_refs.push(id.into());
        self
    }

    pub fn uses_agent(mut self, id_v: impl Into<String>) -> Self {
        self.agent_refs.push(id_v.into());
        self
    }

    pub fn execute<F, Fut>(self, f: F) -> Step<I, O>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        O: JsonSchema + Serialize + Send + Sync + 'static,
        F: Fn(StepContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StepOutcome<O>, OrkError>> + Send + 'static,
    {
        let input_schema = serde_json::to_value(schemars::schema_for!(I)).unwrap_or(Value::Null);
        let output_schema = serde_json::to_value(schemars::schema_for!(O)).unwrap_or(Value::Null);
        let max_attempts = self.retry.as_ref().map(|r| r.max_attempts).unwrap_or(1);
        let to_refs = self.tool_refs.clone();
        let ag_refs = self.agent_refs.clone();
        let timeout = self.timeout;
        let id = self.id.clone();
        let desc = self.description.clone();
        let closure = Arc::new(f);
        let inner: Arc<dyn ErasedStep> = Arc::new(ClosureStep {
            id: id.clone(),
            description: desc,
            input_schema,
            output_schema,
            tool_refs: to_refs,
            agent_refs: ag_refs,
            max_attempts,
            timeout,
            run: Arc::new(
                move |ctx: StepContext,
                      input: Value|
                      -> Pin<
                    Box<dyn Future<Output = Result<StepOutcome<Value>, OrkError>> + Send + 'static>,
                > {
                    let closure = Arc::clone(&closure);
                    Box::pin(async move {
                        let parsed: I = serde_json::from_value(input)
                            .map_err(|e| OrkError::Validation(format!("step input: {e}")))?;
                        let out = closure(ctx, parsed).await?;
                        match out {
                            StepOutcome::Done(o) => {
                                let v = serde_json::to_value(o).map_err(|e| {
                                    OrkError::Internal(format!("step output serialize: {e}"))
                                })?;
                                Ok(StepOutcome::Done(v))
                            }
                            StepOutcome::Suspend {
                                payload,
                                resume_schema,
                            } => Ok(StepOutcome::Suspend {
                                payload,
                                resume_schema,
                            }),
                        }
                    })
                },
            ),
        });
        Step {
            inner,
            _t: PhantomData,
        }
    }
}

type BoxRun = Arc<
    dyn Fn(
            StepContext,
            Value,
        )
            -> Pin<Box<dyn Future<Output = Result<StepOutcome<Value>, OrkError>> + Send + 'static>>
        + Send
        + Sync,
>;

struct ClosureStep {
    id: String,
    #[allow(dead_code)]
    description: String,
    input_schema: Value,
    output_schema: Value,
    tool_refs: Vec<String>,
    agent_refs: Vec<String>,
    max_attempts: u32,
    timeout: Option<Duration>,
    run: BoxRun,
}

#[async_trait]
impl ErasedStep for ClosureStep {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn output_schema(&self) -> Value {
        self.output_schema.clone()
    }

    fn tool_refs(&self) -> &[String] {
        &self.tool_refs
    }

    fn agent_refs(&self) -> &[String] {
        &self.agent_refs
    }

    fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    async fn run(&self, ctx: StepContext, input: Value) -> Result<StepOutcome<Value>, OrkError> {
        (self.run)(ctx, input).await
    }
}

/// Immutable step definition usable in [`WorkflowBuilder::then`](super::workflow::WorkflowBuilder::then).
pub struct Step<I, O> {
    pub(crate) inner: Arc<dyn ErasedStep>,
    pub(crate) _t: PhantomData<(I, O)>,
}

impl<I, O> Clone for Step<I, O> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _t: PhantomData,
        }
    }
}

impl<I, O> From<Step<I, O>> for Arc<dyn ErasedStep> {
    fn from(s: Step<I, O>) -> Self {
        s.inner
    }
}
