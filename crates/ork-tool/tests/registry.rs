//! `OrkApp` registration + lookup (ADR-0051 acceptance).

use ork_app::{McpServerSpec, OrkApp};
use ork_tool::tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
struct ReverseIn {
    input: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReverseOut {
    output: String,
}

fn reverse_tool() -> impl ork_tool::IntoToolDef {
    tool("reverse")
        .description("Reverse the input string")
        .input::<ReverseIn>()
        .output::<ReverseOut>()
        .execute(|_ctx, ReverseIn { input }| async move {
            Ok(ReverseOut {
                output: input.chars().rev().collect(),
            })
        })
}

#[test]
fn ork_app_exposes_registered_tool_schema() {
    let app = OrkApp::builder()
        .tool(reverse_tool())
        .mcp_server("a", McpServerSpec::default())
        .build()
        .expect("builder");

    let def = app.tool("reverse").expect("registered reverse");
    let expected = serde_json::to_value(schemars::schema_for!(ReverseIn)).unwrap();
    assert_eq!(def.input_schema(), &expected);
}
