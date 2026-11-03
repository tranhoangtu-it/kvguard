//! Deterministic ordering for `tools[]` and their JSON Schemas.
//!
//! Implements the key-ordering part of RFC 8785 (JCS): object keys are sorted by Unicode
//! code point. Array order is preserved everywhere except tool lists (sorted by name) and
//! JSON Schema `required` lists, which are sets of property names.

use serde_json::{Map, Value};

/// Sort tools by name and canonicalize their schemas. Returns a description when anything changed.
pub fn canonicalize_tools(body: &mut Value) -> Option<String> {
    let tools = body.get_mut("tools")?.as_array_mut()?;
    let before = serde_json::to_string(tools).ok()?;

    tools.sort_by(|a, b| tool_name(a).cmp(tool_name(b)));
    for tool in tools.iter_mut() {
        canonicalize_tool_schemas(tool);
    }

    let after = serde_json::to_string(tools).ok()?;
    (before != after).then(|| format!("sorted {} tools", tools.len()))
}

/// Tool name for Anthropic (`name`), OpenAI Responses (`name`) and Chat (`function.name`).
pub fn tool_name(tool: &Value) -> &str {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
        .unwrap_or("")
}

fn canonicalize_tool_schemas(tool: &mut Value) {
    for pointer in ["/input_schema", "/parameters", "/function/parameters"] {
        if let Some(schema) = tool.pointer_mut(pointer) {
            *schema = canonical_schema(schema);
        }
    }
}

/// Return a copy of a JSON Schema with sorted object keys and sorted `required` lists.
pub fn canonical_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let sorted: Map<String, Value> = keys
                .into_iter()
                .map(|k| {
                    let v = &map[k];
                    let canonical = if k == "required" {
                        sorted_string_set(v)
                    } else {
                        canonical_schema(v)
                    };
                    (k.clone(), canonical)
                })
                .collect();
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_schema).collect()),
        other => other.clone(),
    }
}

fn sorted_string_set(value: &Value) -> Value {
    match value.as_array() {
        Some(items) if items.iter().all(Value::is_string) => {
            let mut names: Vec<Value> = items.clone();
            names.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            Value::Array(names)
        }
        _ => canonical_schema(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_tools_and_schema_keys_but_not_enums() {
        let mut body = json!({"tools": [
            {"name": "b", "input_schema": {"type": "object", "required": ["z", "a"],
                "properties": {"z": {"type": "string", "enum": ["y", "x"]}, "a": {"type": "number"}}}},
            {"name": "a", "input_schema": {"type": "object"}}
        ]});
        let action = canonicalize_tools(&mut body);
        assert_eq!(action.as_deref(), Some("sorted 2 tools"));
        assert_eq!(body["tools"][0]["name"], "a");
        let schema = serde_json::to_string(&body["tools"][1]["input_schema"]).unwrap();
        assert_eq!(
            schema,
            r#"{"properties":{"a":{"type":"number"},"z":{"enum":["y","x"],"type":"string"}},"required":["a","z"],"type":"object"}"#
        );
    }

    #[test]
    fn already_canonical_tools_report_no_change() {
        let mut body = json!({"tools": [{"name": "a", "input_schema": {"type": "object"}}]});
        assert!(canonicalize_tools(&mut body).is_none());
    }
}
