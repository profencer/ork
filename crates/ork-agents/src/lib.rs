#[cfg(not(feature = "rig-engine"))]
compile_error!(
    "crate `ork-agents` requires feature `rig-engine` (enabled by default). \
     Build with `--features rig-engine`."
);

mod rig_engine;

pub mod code_agent;
pub mod hooks;
pub mod instruction_spec;
/// Hand-rolled, low-level agent. Kept as the legacy escape hatch for callers who
/// need behaviour outside the [`code_agent::CodeAgent`] builder shape (e.g. custom
/// history seeding, bespoke streaming). The builder is the *primary* authoring
/// path — see ADR [`0052`](../../docs/adrs/0052-code-first-agent-dsl.md).
pub mod local;
pub mod model_spec;
pub mod registry;
pub mod roles;
pub mod tool_catalog;

pub use code_agent::{CodeAgent, CodeAgentBuilder};
pub use hooks::{CompletionHook, ToolHook, ToolHookAction};
pub use instruction_spec::InstructionSpec;
pub use model_spec::ModelSpec;
