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

#[test]
fn parameters_schema_matches_schemars_roundtrip() {
    let t = tool("reverse")
        .description("Reverse the input string")
        .input::<ReverseIn>()
        .output::<ReverseOut>()
        .execute(|_ctx, ReverseIn { input }| async move {
            Ok(ReverseOut {
                output: input.chars().rev().collect(),
            })
        });
    let expected = serde_json::to_value(schemars::schema_for!(ReverseIn)).unwrap();
    assert_eq!(t.parameters_schema(), expected);
}
