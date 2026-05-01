use std::collections::HashMap;

use ork_common::error::OrkError;
use serde_json::Value;

pub(crate) fn parse_json_array(s: &str) -> Result<Vec<Value>, OrkError> {
    let trimmed = strip_json_fence(s.trim());
    serde_json::from_str(trimmed).map_err(|e| {
        OrkError::Workflow(format!(
            "for_each resolved template is not a JSON array: {e}; value was: {}",
            trimmed.chars().take(200).collect::<String>()
        ))
    })
}

/// Strip optional ```json ... ``` wrapper from model output.
pub(crate) fn strip_json_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start_matches('`');
        let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
        return rest.trim();
    }
    s
}

pub fn resolve_template(
    template: &str,
    step_outputs: &HashMap<String, String>,
    workflow_input: &Value,
    iteration: Option<(&str, &Value)>,
) -> String {
    let mut result = template.to_string();
    while let Some(start) = result.find("{{") {
        let Some(rel_end) = result[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + rel_end;
        let inner = result[start + 2..end].trim();
        let token = result[start..end + 2].to_string();
        let Some(replacement) = resolve_placeholder(inner, step_outputs, workflow_input, iteration)
        else {
            break;
        };
        if replacement == token {
            break;
        }
        result.replace_range(start..end + 2, &replacement);
    }
    result
}

pub(crate) fn resolve_placeholder(
    path: &str,
    step_outputs: &HashMap<String, String>,
    workflow_input: &Value,
    iteration: Option<(&str, &Value)>,
) -> Option<String> {
    const OUTPUT_SUFFIX: &str = ".output";
    if let Some(rest) = path.strip_prefix("input.") {
        return navigate_to_string(workflow_input, rest);
    }
    if let Some((var, value)) = iteration {
        let prefix = format!("{var}.");
        if path.starts_with(&prefix) {
            return navigate_to_string(value, &path[prefix.len()..]);
        }
        if path == var {
            return Some(value.to_string());
        }
    }
    if let Some(dot_out) = path.find(OUTPUT_SUFFIX) {
        let step_id = &path[..dot_out];
        let after = &path[dot_out + OUTPUT_SUFFIX.len()..];
        let raw = step_outputs.get(step_id)?;
        if after.is_empty() || after == "." {
            return Some(raw.clone());
        }
        let suffix = after.trim_start_matches('.');
        let value = parse_step_json(raw)?;
        return navigate_to_string(&value, suffix);
    }
    None
}

/// Remove `<think>...</think>` (and similar) so JSON can be parsed.
fn strip_redacted_thinking(s: &str) -> String {
    let mut out = s.to_string();
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    while let Some(start) = out.find(OPEN) {
        let Some(rel) = out[start + OPEN.len()..].find(CLOSE) else {
            out.truncate(start);
            break;
        };
        let end = start + OPEN.len() + rel + CLOSE.len();
        out = out[..start].to_string() + &out[end..];
    }
    out
}

/// Find a ```json ... ``` (or ``` ... ```) block anywhere in `s`.
fn extract_markdown_json_block(s: &str) -> Option<&str> {
    if let Some(i) = s.find("```json") {
        let after = s[i + "```json".len()..].trim_start();
        let end = after.find("```")?;
        return Some(after[..end].trim());
    }
    if let Some(i) = s.find("```") {
        let mut after = &s[i + 3..];
        after = after
            .strip_prefix("json")
            .map(|r| r.trim_start())
            .unwrap_or(after.trim_start());
        let end = after.find("```")?;
        let slice = after[..end].trim();
        if slice.starts_with('{') || slice.starts_with('[') {
            return Some(slice);
        }
    }
    None
}

/// First balanced `{ ... }` slice, respecting strings and escapes.
fn extract_first_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let slice = &s[start..];
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, b) in slice.bytes().enumerate() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            if b == b'\\' {
                escape = true;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&slice[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn parse_step_json(raw: &str) -> Option<Value> {
    let cleaned = strip_redacted_thinking(raw);
    let trimmed = cleaned.trim();
    let t = strip_json_fence(trimmed);
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    if let Some(slice) = extract_markdown_json_block(trimmed)
        && let Ok(v) = serde_json::from_str::<Value>(slice)
    {
        return Some(v);
    }
    if let Some(slice) = extract_first_json_object(trimmed)
        && let Ok(v) = serde_json::from_str::<Value>(slice)
    {
        return Some(v);
    }
    None
}

fn navigate_to_string(root: &Value, path: &str) -> Option<String> {
    if path.is_empty() {
        return value_as_display(root);
    }
    let mut cur = root;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        cur = if let Ok(i) = part.parse::<usize>() {
            cur.get(i)?
        } else {
            cur.get(part)?
        };
    }
    value_as_display(cur)
}

fn value_as_display(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => Some(String::new()),
        _ => Some(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_input_and_nested_output() {
        let mut steps = HashMap::new();
        steps.insert(
            "select_repos".into(),
            r#"{"repos":[{"name":"a"}],"query":"rate"}"#.into(),
        );
        let input = json!({"task": "hello"});
        let t = resolve_template(
            "Task: {{input.task}} Q: {{select_repos.output.query}}",
            &steps,
            &input,
            None,
        );
        assert_eq!(t, "Task: hello Q: rate");
    }

    #[test]
    fn resolve_iteration() {
        let steps = HashMap::new();
        let input = json!({});
        let item = json!({"name":"r1","reason":"x"});
        let t = resolve_template("Repo {{repo.name}}", &steps, &input, Some(("repo", &item)));
        assert_eq!(t, "Repo r1");
    }

    #[test]
    fn resolve_unresolved_placeholder_stops_without_hang() {
        let steps = HashMap::new();
        let input = json!({});
        let t = resolve_template("x {{missing.output.field}} y", &steps, &input, None);
        assert_eq!(t, "x {{missing.output.field}} y");
    }

    #[test]
    fn parse_step_json_strips_thinking_and_fenced_json() {
        let raw = r#"<think>notes here</think>

```json
{"repos":[{"name":"kong"}],"query":"rate limit"}
```
"#;
        let v = parse_step_json(raw).expect("parsed");
        assert_eq!(v["query"], "rate limit");
        assert_eq!(v["repos"][0]["name"], "kong");
    }

    #[test]
    fn parse_step_json_finds_object_after_thinking_without_fence() {
        let raw = r#"<think>thinking</think>

{"repos":[],"query":"q"}
"#;
        let v = parse_step_json(raw).expect("parsed");
        assert_eq!(v["query"], "q");
    }
}
