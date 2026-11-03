//! Locates the first changed JSON path between two requests and classifies the mutation.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::MutationKind;
use crate::canonicalizer::jcs::{canonical_schema, tool_name};
use crate::canonicalizer::sanitizer::{has_billing_header, mask_timestamps, strip_billing_header};
use crate::lineage::block_hasher::{BlockKind, ContextBlock};
use crate::lineage::lcp_tree::{Snapshot, common_prefix_len};

const SNIPPET_CHARS: usize = 160;

/// UUIDs and long hex runs (hashes, request ids).
static VOLATILE_IDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b|\b[0-9a-f]{8,}\b",
    )
    .expect("valid regex")
});

/// First structural divergence between parent and current request.
#[derive(Debug, Clone)]
pub struct AstDivergence {
    pub block_index: usize,
    pub json_path: String,
    pub kind: MutationKind,
    pub old_snippet: String,
    pub new_snippet: String,
}

/// Returns `None` when the parent is a block-prefix of the current request.
pub fn first_divergence(parent: &Snapshot, current: &Snapshot) -> Option<AstDivergence> {
    let index = common_prefix_len(&parent.blocks, &current.blocks);
    let old_block = parent.blocks.get(index)?;
    let new_block = current.blocks.get(index)?;

    let old_value: Value = serde_json::from_str(&old_block.text).unwrap_or(Value::Null);
    let new_value: Value = serde_json::from_str(&new_block.text).unwrap_or(Value::Null);
    let (json_path, old_snippet, new_snippet) =
        first_diff_path(&old_value, &new_value, &old_block.path)
            .unwrap_or_else(|| (old_block.path.clone(), String::new(), String::new()));

    let kind = classify(parent, current, old_block, new_block, &old_value, &new_value);
    Some(AstDivergence {
        block_index: index,
        json_path,
        kind,
        old_snippet,
        new_snippet,
    })
}

fn classify(
    parent: &Snapshot,
    current: &Snapshot,
    old_block: &ContextBlock,
    new_block: &ContextBlock,
    old_value: &Value,
    new_value: &Value,
) -> MutationKind {
    if old_block.kind == BlockKind::Tool || new_block.kind == BlockKind::Tool {
        return classify_tools(parent, current, old_value, new_value);
    }
    match old_block.kind {
        BlockKind::System => classify_text_change(old_value, new_value)
            .unwrap_or(MutationKind::SystemPromptChanged),
        BlockKind::Message => MutationKind::HistoricalMessageMutated,
        BlockKind::Tool => MutationKind::ToolDefinitionChanged,
    }
}

fn classify_tools(
    parent: &Snapshot,
    current: &Snapshot,
    old_value: &Value,
    new_value: &Value,
) -> MutationKind {
    let names = |snap: &Snapshot| -> Vec<String> {
        snap.body
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| tools.iter().map(|t| tool_name(t).to_string()).collect())
            .unwrap_or_default()
    };
    let (old_names, new_names) = (names(parent), names(current));
    if old_names != new_names {
        let mut a = old_names;
        let mut b = new_names;
        a.sort();
        b.sort();
        return if a == b {
            MutationKind::ToolOrderShuffled
        } else {
            MutationKind::ToolSetChanged
        };
    }
    if canonical_schema(old_value) == canonical_schema(new_value) {
        return MutationKind::SchemaKeyOrderChanged;
    }
    classify_text_change(old_value, new_value).unwrap_or(MutationKind::ToolDefinitionChanged)
}

/// Detect volatile-noise changes inside free text (system prompts, tool descriptions).
fn classify_text_change(old_value: &Value, new_value: &Value) -> Option<MutationKind> {
    let old_text = collect_strings(old_value);
    let new_text = collect_strings(new_value);
    if (has_billing_header(&old_text) || has_billing_header(&new_text))
        && strip_billing_header(&old_text) == strip_billing_header(&new_text)
    {
        return Some(MutationKind::VolatileSystemHeader);
    }
    if mask_timestamps(&old_text) == mask_timestamps(&new_text) {
        return Some(MutationKind::DynamicTimestampInPrefix);
    }
    if VOLATILE_IDS.replace_all(&old_text, "<id>") == VOLATILE_IDS.replace_all(&new_text, "<id>") {
        return Some(MutationKind::VolatileSystemHeader);
    }
    None
}

/// All string leaves of a value, newline-joined (keys excluded).
fn collect_strings(value: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}

/// Depth-first search (in serialization order) for the first differing JSON path.
pub fn first_diff_path(a: &Value, b: &Value, path: &str) -> Option<(String, String, String)> {
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            for ((ka, va), (kb, vb)) in ma.iter().zip(mb.iter()) {
                if ka != kb {
                    return Some((
                        path.to_string(),
                        format!("key order: …{ka}…"),
                        format!("key order: …{kb}…"),
                    ));
                }
                if let Some(found) = first_diff_path(va, vb, &format!("{path}.{ka}")) {
                    return Some(found);
                }
            }
            (ma.len() != mb.len()).then(|| (path.to_string(), snippet(a), snippet(b)))
        }
        (Value::Array(xa), Value::Array(xb)) => {
            for (i, (va, vb)) in xa.iter().zip(xb.iter()).enumerate() {
                if let Some(found) = first_diff_path(va, vb, &format!("{path}[{i}]")) {
                    return Some(found);
                }
            }
            (xa.len() != xb.len()).then(|| {
                (
                    format!("{path}[{}]", xa.len().min(xb.len())),
                    snippet(a),
                    snippet(b),
                )
            })
        }
        _ => (a != b).then(|| (path.to_string(), snippet(a), snippet(b))),
    }
}

fn snippet(v: &Value) -> String {
    let text = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.chars().count() > SNIPPET_CHARS {
        let cut: String = text.chars().take(SNIPPET_CHARS).collect();
        format!("{cut}…")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ApiKind;
    use serde_json::json;
    use std::sync::Arc;

    fn snap(v: Value) -> Snapshot {
        Snapshot::new(ApiKind::AnthropicMessages, Arc::new(v))
    }

    #[test]
    fn finds_nested_path_in_history() {
        let a = snap(json!({"system": "S", "messages": [
            {"role": "user", "content": [{"type": "text", "text": "old"}]},
            {"role": "assistant", "content": "x"}]}));
        let b = snap(json!({"system": "S", "messages": [
            {"role": "user", "content": [{"type": "text", "text": "new"}]},
            {"role": "assistant", "content": "x"}]}));
        let d = first_divergence(&a, &b).expect("divergence");
        assert_eq!(d.json_path, "messages[0].content[0].text");
        assert_eq!(d.kind, MutationKind::HistoricalMessageMutated);
    }

    #[test]
    fn timestamp_change_is_classified() {
        let a = snap(json!({"system": "Time 2026-11-03T22:40:19Z",
            "messages": [{"role": "user", "content": "q"}]}));
        let b = snap(json!({"system": "Time 2026-11-03T22:41:00Z",
            "messages": [{"role": "user", "content": "q"}]}));
        let d = first_divergence(&a, &b).expect("divergence");
        assert_eq!(d.kind, MutationKind::DynamicTimestampInPrefix);
        assert_eq!(d.json_path, "system");
    }
}
