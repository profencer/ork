//! [`IntoToolDef`](IntoToolDef) — ergonomic `Arc<dyn ToolDef>` from concrete [`ToolDef`] values.

use std::sync::Arc;

use ork_core::ports::tool_def::ToolDef;

/// Convert a [`ToolDef`] value into `Arc<dyn ToolDef>` for [`OrkApp::builder().tool`](../../ork-app/src/builder.rs).
///
/// Note: no `impl` for [`Arc`](std::sync::Arc)`<dyn ToolDef>` — it would overlap the blanket under
/// Rust's coherence rules.
pub trait IntoToolDef {
    fn into_tool_def(self) -> Arc<dyn ToolDef>;
}

impl<T: ToolDef + 'static> IntoToolDef for T {
    fn into_tool_def(self) -> Arc<dyn ToolDef> {
        Arc::new(self)
    }
}
