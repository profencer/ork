//! Built-in embed handlers (ADR-0015).

mod datetime;
mod math;
mod status_update;
mod uuid;
mod var;

pub use datetime::DateTimeHandler;
pub use math::MathHandler;
pub use status_update::StatusUpdateHandler;
pub use uuid::UuidHandler;
pub use var::VarHandler;

use super::EmbedRegistry;

/// Registers the default handler set on `registry`.
pub fn register_builtins(reg: &mut EmbedRegistry) {
    reg.register(MathHandler);
    reg.register(DateTimeHandler);
    reg.register(UuidHandler);
    reg.register(VarHandler);
    reg.register(StatusUpdateHandler::default());
}
