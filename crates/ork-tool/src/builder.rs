//! Typestate builder for [`super::Tool`] (ADR [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md)).

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ork_common::error::OrkError;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ToolContext;
use crate::retry::RetryPolicy;
use crate::tool::Tool;

/// Type-state: input/output types not wired yet.
#[derive(Debug, Clone, Copy)]
pub struct Underspec;

/// Start building a tool with id `id` (must satisfy `OrkApp` id validation at registration).
#[must_use]
pub fn tool(id: impl Into<String>) -> ToolBuilder<Underspec, Underspec> {
    ToolBuilder {
        id: id.into(),
        description: String::new(),
        timeout: None,
        retry: None,
        fatal_on: None,
        gate: None,
        _i: PhantomData,
        _o: PhantomData,
    }
}

pub struct ToolBuilder<I, O> {
    id: String,
    description: String,
    timeout: Option<Duration>,
    retry: Option<RetryPolicy>,
    fatal_on: Option<Arc<dyn Fn(&OrkError) -> bool + Send + Sync>>,
    gate: Option<Arc<dyn Fn(&ToolContext) -> bool + Send + Sync>>,
    _i: PhantomData<I>,
    _o: PhantomData<O>,
}

impl<I, O> ToolBuilder<I, O> {
    #[must_use]
    pub fn description(mut self, s: impl Into<String>) -> Self {
        self.description = s.into();
        self
    }

    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    #[must_use]
    pub fn retry(mut self, p: RetryPolicy) -> Self {
        self.retry = Some(p);
        self
    }

    /// Marks additional error variants as fatal for the rig loop (ADR-0051 §`Failure model`).
    #[must_use]
    pub fn fatal_on<F>(mut self, f: F) -> Self
    where
        F: Fn(&OrkError) -> bool + Send + Sync + 'static,
    {
        self.fatal_on = Some(Arc::new(f));
        self
    }

    /// Omit from LLM-visible catalog when predicate is false (ADR-0051 §`dynamic_tools`).
    #[must_use]
    pub fn gate<F>(mut self, f: F) -> Self
    where
        F: Fn(&ToolContext) -> bool + Send + Sync + 'static,
    {
        self.gate = Some(Arc::new(f));
        self
    }
}

impl ToolBuilder<Underspec, Underspec> {
    #[must_use]
    pub fn input<I>(self) -> ToolBuilder<I, Underspec>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    {
        ToolBuilder {
            id: self.id,
            description: self.description,
            timeout: self.timeout,
            retry: self.retry,
            fatal_on: self.fatal_on,
            gate: self.gate,
            _i: PhantomData,
            _o: PhantomData,
        }
    }

    #[must_use]
    pub fn output<O>(self) -> ToolBuilder<Underspec, O>
    where
        O: JsonSchema + Serialize + Send + Sync + 'static,
    {
        ToolBuilder {
            id: self.id,
            description: self.description,
            timeout: self.timeout,
            retry: self.retry,
            fatal_on: self.fatal_on,
            gate: self.gate,
            _i: PhantomData,
            _o: PhantomData,
        }
    }
}

impl<O> ToolBuilder<Underspec, O>
where
    O: JsonSchema + Serialize + Send + Sync + 'static,
{
    #[must_use]
    pub fn input<I>(self) -> ToolBuilder<I, O>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    {
        ToolBuilder {
            id: self.id,
            description: self.description,
            timeout: self.timeout,
            retry: self.retry,
            fatal_on: self.fatal_on,
            gate: self.gate,
            _i: PhantomData,
            _o: PhantomData,
        }
    }
}

impl<I> ToolBuilder<I, Underspec>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
{
    #[must_use]
    pub fn output<O>(self) -> ToolBuilder<I, O>
    where
        O: JsonSchema + Serialize + Send + Sync + 'static,
    {
        ToolBuilder {
            id: self.id,
            description: self.description,
            timeout: self.timeout,
            retry: self.retry,
            fatal_on: self.fatal_on,
            gate: self.gate,
            _i: PhantomData,
            _o: PhantomData,
        }
    }
}

impl<I, O> ToolBuilder<I, O>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    O: JsonSchema + Serialize + Send + Sync + 'static,
{
    #[must_use]
    pub fn execute<F, Fut>(self, f: F) -> Tool<I, O>
    where
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, OrkError>> + Send + 'static,
    {
        let input_schema = serde_json::to_value(schemars::schema_for!(I)).unwrap_or(Value::Null);
        let output_schema = serde_json::to_value(schemars::schema_for!(O)).unwrap_or(Value::Null);
        let run: Arc<
            dyn Fn(ToolContext, I) -> Pin<Box<dyn Future<Output = Result<O, OrkError>> + Send>>
                + Send
                + Sync,
        > = Arc::new(move |ctx, input| Box::pin(f(ctx, input)));
        Tool::new(
            self.id,
            self.description,
            input_schema,
            output_schema,
            run,
            self.fatal_on,
            self.gate,
            self.timeout,
            self.retry,
        )
    }
}
