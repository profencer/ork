//! Materialized [`Tool`] value (ADR [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md)).

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::tool_def::{ToolDef, default_fatal_tool_error};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ToolContext;
use crate::retry::RetryPolicy;

type BoxRun<I, O> = Arc<
    dyn Fn(ToolContext, I) -> Pin<Box<dyn Future<Output = Result<O, OrkError>> + Send>>
        + Send
        + Sync,
>;

/// Typed native tool: one closure + cached JSON Schemas.
pub struct Tool<I, O> {
    id: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
    run: BoxRun<I, O>,
    fatal_on: Option<Arc<dyn Fn(&OrkError) -> bool + Send + Sync>>,
    gate: Option<Arc<dyn Fn(&ToolContext) -> bool + Send + Sync>>,
    timeout: Option<Duration>,
    retry: Option<RetryPolicy>,
    _p: PhantomData<(I, O)>,
}

impl<I, O> Tool<I, O> {
    pub(crate) fn new(
        id: String,
        description: String,
        input_schema: Value,
        output_schema: Value,
        run: BoxRun<I, O>,
        fatal_on: Option<Arc<dyn Fn(&OrkError) -> bool + Send + Sync>>,
        gate: Option<Arc<dyn Fn(&ToolContext) -> bool + Send + Sync>>,
        timeout: Option<Duration>,
        retry: Option<RetryPolicy>,
    ) -> Self {
        Self {
            id,
            description,
            input_schema,
            output_schema,
            run,
            fatal_on,
            gate,
            timeout,
            retry,
            _p: PhantomData,
        }
    }

    #[must_use]
    pub fn parameters_schema(&self) -> Value {
        self.input_schema.clone()
    }

    #[must_use]
    pub fn output_schema_value(&self) -> Value {
        self.output_schema.clone()
    }
}

impl<I, O> Clone for Tool<I, O> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            run: self.run.clone(),
            fatal_on: self.fatal_on.clone(),
            gate: self.gate.clone(),
            timeout: self.timeout,
            retry: self.retry.clone(),
            _p: PhantomData,
        }
    }
}

#[async_trait]
impl<I, O> ToolDef for Tool<I, O>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    O: JsonSchema + Serialize + Send + Sync + 'static,
{
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    fn output_schema(&self) -> &Value {
        &self.output_schema
    }

    async fn invoke(&self, ctx: &AgentContext, input: &Value) -> Result<Value, OrkError> {
        let max = self.retry.as_ref().map_or(1, |r| r.max_attempts.max(1));
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            let parsed: I = serde_json::from_value(input.clone())
                .map_err(|e| OrkError::Validation(format!("tool `{}` input: {e}", self.id)))?;
            let tctx = ToolContext::from_agent_context(ctx.clone());
            let fut = (self.run)(tctx, parsed);
            let out = if let Some(d) = self.timeout {
                tokio::time::timeout(d, fut)
                    .await
                    .map_err(|_| OrkError::Internal(format!("tool `{}` timed out", self.id)))?
            } else {
                fut.await
            };

            match out {
                Ok(v) => {
                    return serde_json::to_value(&v).map_err(|e| {
                        OrkError::Internal(format!(
                            "tool `{}` failed to serialise output: {e}",
                            self.id
                        ))
                    });
                }
                Err(e) => {
                    if attempt >= max {
                        return Err(e);
                    }
                    let Some(r) = &self.retry else {
                        return Err(e);
                    };
                    let mut sleep = r.backoff.initial;
                    for _ in 1..attempt {
                        let next = (sleep.as_secs_f64() * r.backoff.multiplier)
                            .min(r.backoff.max.as_secs_f64());
                        sleep = Duration::from_secs_f64(next);
                    }
                    sleep = sleep.saturating_add(r.backoff.jitter);
                    tokio::time::sleep(sleep).await;
                }
            }
        }
    }

    fn is_fatal(&self, err: &OrkError) -> bool {
        if let Some(f) = &self.fatal_on
            && f(err)
        {
            return true;
        }
        default_fatal_tool_error(err)
    }

    fn visible(&self, ctx: &AgentContext) -> bool {
        if let Some(g) = &self.gate {
            let tctx = ToolContext::from_agent_context(ctx.clone());
            return g(&tctx);
        }
        true
    }
}

/// Dynamic dispatch wrapper used when a tool body is already captured in an `Arc<dyn ToolDef>`.
pub struct DynToolInvoke {
    id: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
    invoke_fn: Arc<
        dyn Fn(AgentContext, Value) -> Pin<Box<dyn Future<Output = Result<Value, OrkError>> + Send>>
            + Send
            + Sync,
    >,
    is_fatal_fn: Option<Arc<dyn Fn(&OrkError) -> bool + Send + Sync>>,
    visible_fn: Option<Arc<dyn Fn(&AgentContext) -> bool + Send + Sync>>,
    /// When set, [`ToolDef::is_fatal`] always returns `false` (ADR-0010 MCP / non-fatal tool plane).
    force_non_fatal: bool,
}

impl DynToolInvoke {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        output_schema: Value,
        invoke_fn: Arc<
            dyn Fn(
                    AgentContext,
                    Value,
                ) -> Pin<Box<dyn Future<Output = Result<Value, OrkError>> + Send>>
                + Send
                + Sync,
        >,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            input_schema,
            output_schema,
            invoke_fn,
            is_fatal_fn: None,
            visible_fn: None,
            force_non_fatal: false,
        }
    }

    /// All errors from this tool are treated as non-fatal in the rig loop (tool result JSON),
    /// including upstream [`default_fatal_tool_error`] cases.
    #[must_use]
    pub fn force_non_fatal(mut self) -> Self {
        self.force_non_fatal = true;
        self
    }

    #[must_use]
    pub fn with_fatal_on(mut self, f: impl Fn(&OrkError) -> bool + Send + Sync + 'static) -> Self {
        self.is_fatal_fn = Some(Arc::new(f));
        self
    }

    #[must_use]
    pub fn with_visible(
        mut self,
        f: impl Fn(&AgentContext) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.visible_fn = Some(Arc::new(f));
        self
    }
}

#[async_trait]
impl ToolDef for DynToolInvoke {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    fn output_schema(&self) -> &Value {
        &self.output_schema
    }

    async fn invoke(&self, ctx: &AgentContext, input: &Value) -> Result<Value, OrkError> {
        let f = self.invoke_fn.clone();
        let c = ctx.clone();
        let v = input.clone();
        f(c, v).await
    }

    fn is_fatal(&self, err: &OrkError) -> bool {
        if self.force_non_fatal {
            return false;
        }
        if let Some(f) = &self.is_fatal_fn
            && f(err)
        {
            return true;
        }
        default_fatal_tool_error(err)
    }

    fn visible(&self, ctx: &AgentContext) -> bool {
        self.visible_fn.as_ref().is_none_or(|f| f(ctx))
    }
}
