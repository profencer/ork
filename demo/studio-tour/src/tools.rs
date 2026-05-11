//! Two tiny native tools that the demo agents can call. Both run
//! synchronously, take a JSON-Schema-derivable input, and return JSON-
//! Schema-derivable output. Studio's Chat panel will render a
//! `tool_call` chip when the agent invokes either of these via the
//! SSE stream encoded by `ork-api`'s `sse/encoder.rs`.

use chrono::Utc;
use ork_common::error::OrkError;
use ork_tool::{Tool, tool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NowInput {
    /// Optional IANA timezone name. v1 ignores the field; it exists so
    /// reviewers can see the JSON-schema input rendered in Studio.
    #[serde(default)]
    #[allow(dead_code)]
    pub tz: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NowOutput {
    /// Current wall-clock time in RFC-3339 (UTC).
    pub now_utc: String,
}

pub fn now_tool() -> Tool<NowInput, NowOutput> {
    tool("clock-now")
        .description("Return the current UTC time as an RFC-3339 string.")
        .input::<NowInput>()
        .output::<NowOutput>()
        .execute(|_ctx, _input| async move {
            Ok::<NowOutput, OrkError>(NowOutput {
                now_utc: Utc::now().to_rfc3339(),
            })
        })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiceInput {
    /// Number of faces on the die. Defaults to 6.
    #[serde(default = "default_sides")]
    pub sides: u32,
    /// How many dice to roll. Defaults to 1; capped at 32 to keep the
    /// JSON response bounded.
    #[serde(default = "default_count")]
    pub count: u32,
}

fn default_sides() -> u32 {
    6
}

fn default_count() -> u32 {
    1
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiceOutput {
    pub rolls: Vec<u32>,
    pub total: u32,
}

pub fn dice_tool() -> Tool<DiceInput, DiceOutput> {
    tool("dice-roll")
        .description("Roll `count` dice with `sides` faces; return individual rolls + sum.")
        .input::<DiceInput>()
        .output::<DiceOutput>()
        .execute(|_ctx, input| async move {
            let count = input.count.min(32).max(1);
            let sides = input.sides.max(2);
            // Deterministic-ish RNG so demo screencasts are reproducible:
            // seed from the current second + the requested shape. Studio
            // reviewers don't need cryptographic randomness here.
            let seed = (Utc::now().timestamp_millis() as u64)
                .wrapping_add(u64::from(count) * 131)
                .wrapping_add(u64::from(sides) * 17);
            let mut state = seed;
            let mut rolls = Vec::with_capacity(count as usize);
            for _ in 0..count {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let r = ((state >> 33) as u32 % sides) + 1;
                rolls.push(r);
            }
            let total = rolls.iter().sum();
            Ok::<DiceOutput, OrkError>(DiceOutput { rolls, total })
        })
}
