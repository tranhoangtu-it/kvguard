//! Splits a request into ordered context blocks (tools → system → messages) and hashes each.
//!
//! Provider caches match on the serialized prompt prefix, so each block keeps the exact
//! key order it was sent with. `cache_control` markers are removed first: Anthropic does not
//! include them in the cached prefix, and clients move them every turn.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xxhash_rust::xxh64::xxh64;

use crate::provider::ApiKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Tool,
    System,
    Message,
}

/// One cacheable unit of the prompt prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlock {
    pub kind: BlockKind,
    /// JSON path of the block inside the request body, e.g. `tools[3]` or `messages[0]`.
    pub path: String,
    /// Compact JSON serialization (original key order, `cache_control` removed).
    pub text: String,
    pub hash: u64,
}

impl ContextBlock {
    fn new(kind: BlockKind, path: String, value: &Value) -> Self {
        let cleaned = strip_cache_control(value);
        let text = serde_json::to_string(&cleaned).unwrap_or_default();
        let hash = xxh64(text.as_bytes(), 0);
        Self {
            kind,
            path,
            text,
            hash,
        }
    }
}

/// Return a copy of `value` without any `cache_control` keys (at any depth).
pub fn strip_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip_cache_control(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_cache_control).collect()),
        other => other.clone(),
    }
}

/// Decompose a request body into prompt-ordered blocks.
pub fn decompose(api: ApiKind, body: &Value) -> Vec<ContextBlock> {
    let mut blocks = array_blocks(body, "tools", |_| BlockKind::Tool);
    match api {
        ApiKind::AnthropicMessages => {
            blocks.extend(anthropic_system_blocks(body));
            blocks.extend(array_blocks(body, "messages", |_| BlockKind::Message));
        }
        ApiKind::OpenAiChat | ApiKind::OllamaChat => {
            // System/developer messages live in the conversation array but are classified
            // as system blocks so diagnostics can name them correctly.
            blocks.extend(array_blocks(body, "messages", role_kind));
        }
        ApiKind::OpenAiResponses => {
            if let Some(instructions) = body.get("instructions").filter(|v| !v.is_null()) {
                blocks.push(ContextBlock::new(
                    BlockKind::System,
                    "instructions".into(),
                    instructions,
                ));
            }
            match body.get("input") {
                Some(Value::Array(_)) => blocks.extend(array_blocks(body, "input", role_kind)),
                Some(input @ Value::String(_)) => {
                    blocks.push(ContextBlock::new(BlockKind::Message, "input".into(), input));
                }
                _ => {}
            }
        }
    }
    blocks
}

fn role_kind(item: &Value) -> BlockKind {
    match item.get("role").and_then(Value::as_str) {
        Some("system" | "developer") => BlockKind::System,
        _ => BlockKind::Message,
    }
}

fn array_blocks(body: &Value, key: &str, kind: impl Fn(&Value) -> BlockKind) -> Vec<ContextBlock> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(i, item)| ContextBlock::new(kind(item), format!("{key}[{i}]"), item))
                .collect()
        })
        .unwrap_or_default()
}

fn anthropic_system_blocks(body: &Value) -> Vec<ContextBlock> {
    match body.get("system") {
        Some(s @ Value::String(_)) => {
            vec![ContextBlock::new(BlockKind::System, "system".into(), s)]
        }
        Some(Value::Array(_)) => array_blocks(body, "system", |_| BlockKind::System),
        _ => Vec::new(),
    }
}

/// Concatenate block texts in prompt order — the string used for byte/token divergence.
pub fn prompt_text(blocks: &[ContextBlock]) -> String {
    let capacity = blocks.iter().map(|b| b.text.len() + 1).sum();
    let mut out = String::with_capacity(capacity);
    for block in blocks {
        out.push_str(&block.text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_order_is_tools_system_messages() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "system": [{"type": "text", "text": "sys"}],
            "tools": [{"name": "a"}, {"name": "b"}]
        });
        let blocks = decompose(ApiKind::AnthropicMessages, &body);
        let paths: Vec<_> = blocks.iter().map(|b| b.path.as_str()).collect();
        assert_eq!(paths, ["tools[0]", "tools[1]", "system[0]", "messages[0]"]);
    }

    #[test]
    fn cache_control_does_not_change_hash() {
        let a = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "x"}]}]});
        let b = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}
        ]}]});
        let ha = decompose(ApiKind::AnthropicMessages, &a)[0].hash;
        let hb = decompose(ApiKind::AnthropicMessages, &b)[0].hash;
        assert_eq!(ha, hb);
    }

    #[test]
    fn openai_system_role_is_system_block() {
        let body = json!({"messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"}
        ]});
        let blocks = decompose(ApiKind::OpenAiChat, &body);
        assert_eq!(blocks[0].kind, BlockKind::System);
        assert_eq!(blocks[1].kind, BlockKind::Message);
    }
}
