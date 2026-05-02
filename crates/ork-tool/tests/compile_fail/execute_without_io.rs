/// Underspecified builder: `.execute` is only on `ToolBuilder` after `.input` + `.output`.
use ork_tool::tool;

fn main() {
    let _ = tool("x")
        .description("d")
        .execute(|_, ()| async {
            Result::<(), ork_common::error::OrkError>::Ok(())
        });
}
