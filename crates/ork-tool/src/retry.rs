//! Retry policy for tool execution (shape aligned with `ork-workflow`).

use std::time::Duration;

/// Bounded retries for a tool invocation.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: ExponentialBackoff,
}

#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    pub initial: Duration,
    pub multiplier: f64,
    pub jitter: Duration,
    pub max: Duration,
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            multiplier: 2.0,
            jitter: Duration::from_millis(50),
            max: Duration::from_secs(30),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff: ExponentialBackoff::default(),
        }
    }
}
