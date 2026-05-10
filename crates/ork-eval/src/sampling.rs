//! Sampling predicates for live scorers
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Live sampling`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// When a live-attached scorer should fire on a completed run.
#[derive(Clone, Debug)]
pub enum Sampling {
    /// Run on each completion with probability `rate ∈ [0, 1]`.
    Ratio { rate: f32 },
    /// Run at most `n` times per rolling 60-second window. Excess
    /// completions skip scoring (and are *not* counted as drops —
    /// drops are reserved for queue-full conditions).
    PerMinute { n: u32 },
    /// Fire only when the run errored out.
    OnError,
    /// Never fire live (offline-only when paired with `ScorerSpec::Both`).
    Never,
}

impl Sampling {
    /// Decide whether to fire on a completion.
    ///
    /// `errored` is `true` for runs that produced an `OrkError` — used
    /// by `OnError`.
    #[must_use]
    pub fn should_fire(&self, errored: bool, state: &SamplingState) -> bool {
        match self {
            Self::Ratio { rate } => {
                let r = rate.clamp(0.0, 1.0);
                if r <= 0.0 {
                    false
                } else if r >= 1.0 {
                    true
                } else {
                    fastrand::f32() < r
                }
            }
            Self::PerMinute { n } => state.try_take_per_minute(*n),
            Self::OnError => errored,
            Self::Never => false,
        }
    }
}

/// Mutable state companion to [`Sampling`]. Owned per binding so each
/// `(target, scorer)` pair tracks its own token-bucket window.
#[derive(Debug, Default)]
pub struct SamplingState {
    bucket: Mutex<TokenBucket>,
}

#[derive(Debug, Default)]
struct TokenBucket {
    /// First `Instant` of the current 60-second window.
    window_start: Option<Instant>,
    /// Calls already accepted in the current window.
    used: u32,
}

impl SamplingState {
    fn try_take_per_minute(&self, capacity: u32) -> bool {
        if capacity == 0 {
            return false;
        }
        let mut bucket = self.bucket.lock().expect("sampling bucket poisoned");
        let now = Instant::now();
        let in_window = matches!(
            bucket.window_start,
            Some(start) if now.duration_since(start) < Duration::from_secs(60)
        );
        if !in_window {
            bucket.window_start = Some(now);
            bucket.used = 0;
        }
        if bucket.used < capacity {
            bucket.used += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_does_not_fire() {
        let s = Sampling::Never;
        assert!(!s.should_fire(false, &SamplingState::default()));
        assert!(!s.should_fire(true, &SamplingState::default()));
    }

    #[test]
    fn on_error_fires_only_when_errored() {
        let s = Sampling::OnError;
        assert!(!s.should_fire(false, &SamplingState::default()));
        assert!(s.should_fire(true, &SamplingState::default()));
    }

    #[test]
    fn ratio_zero_never_fires() {
        let s = Sampling::Ratio { rate: 0.0 };
        for _ in 0..200 {
            assert!(!s.should_fire(false, &SamplingState::default()));
        }
    }

    #[test]
    fn ratio_one_always_fires() {
        let s = Sampling::Ratio { rate: 1.0 };
        for _ in 0..200 {
            assert!(s.should_fire(false, &SamplingState::default()));
        }
    }

    #[test]
    fn per_minute_caps_within_window() {
        let s = Sampling::PerMinute { n: 3 };
        let state = SamplingState::default();
        assert!(s.should_fire(false, &state));
        assert!(s.should_fire(false, &state));
        assert!(s.should_fire(false, &state));
        assert!(!s.should_fire(false, &state));
    }

    #[test]
    fn per_minute_zero_never_fires() {
        let s = Sampling::PerMinute { n: 0 };
        let state = SamplingState::default();
        assert!(!s.should_fire(false, &state));
    }
}
