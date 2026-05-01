#[cfg(not(feature = "rig-engine"))]
compile_error!(
    "crate `ork-agents` requires feature `rig-engine` (enabled by default). \
     Build with `--features rig-engine`."
);

mod rig_engine;

pub mod local;
pub mod registry;
pub mod roles;
pub mod tool_catalog;
