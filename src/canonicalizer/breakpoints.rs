//! Cache hints: Anthropic `cache_control` breakpoints and OpenAI `prompt_cache_key`.

use serde_json::{Value, json};
use xxhash_rust::xxh64::xxh64;

/// Anthropic accepts at most four `cache_control` breakpoints per request.
pub const ANTHROPIC_MAX_BREAKPOINTS: usize = 4;

/// Count existing `cache_control` markers anywhere in tools/system/messages.
pub fn count_breakpoints(body: &Value) -> usize {
    fn count(v: &Value) -> usize {
        match v {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count).sum(),
            _ => 0,
        }
    }
    ["tools", "system", "messages"]
        .iter()
        .filter_map(|k| body.get(*k))
        .map(count)
        .sum()
}

/// Add breakpoints only when the client sent none, so client-managed caching is never
/// overridden and the four-breakpoint limit can't be exceeded.
pub fn add_anthropic_breakpoints(body: &mut Value) -> Option<String> {
    if count_breakpoints(body) > 0 {
        return None;
    }
    let mut added = 0usize;
    if let Some(last_tool) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|t| t.last_mut())
    {
        added += usize::from(mark(last_tool));
    }
    if let Some(system) = body.get_mut("system") {
        if let Value::String(text) = system {
            *system = json!([{"type": "text", "text": text.clone()}]);
        }
        if let Some(last) = system.as_array_mut().and_then(|p| p.last_mut()) {
            added += usize::from(mark(last));
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let len = messages.len();
        // Rolling window: newest message plus the previous user turn.
        let targets = [len.checked_sub(1), len.checked_sub(3)];
        for idx in targets.into_iter().flatten() {
            if added >= ANTHROPIC_MAX_BREAKPOINTS {
                break;
            }
            added += usize::from(mark_message(&mut messages[idx]));
        }
    }
    (added > 0).then(|| format!("added {added} cache_control breakpoint(s)"))
}

fn mark(block: &mut Value) -> bool {
    match block.as_object_mut() {
        Some(map) if !map.contains_key("cache_control") => {
            map.insert("cache_control".into(), json!({"type": "ephemeral"}));
            true
        }
        _ => false,
    }
}

fn mark_message(message: &mut Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Value::String(text) = content {
        if text.is_empty() {
            return false;
        }
        *content = json!([{"type": "text", "text": text.clone()}]);
    }
    content
        .as_array_mut()
        .and_then(|parts| parts.last_mut())
        .is_some_and(mark)
}

/// Set a stable `prompt_cache_key` derived from model + tools + system prefix when absent.
pub fn add_prompt_cache_key(body: &mut Value) -> Option<String> {
    if body.get("prompt_cache_key").is_some() {
        return None;
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tools = body.get("tools").map(Value::to_string).unwrap_or_default();
    let system = system_prefix(body);
    let seed = format!("{model}\u{0}{tools}\u{0}{system}");
    let key = format!("kvg-{:016x}", xxh64(seed.as_bytes(), 0));
    body.as_object_mut()?
        .insert("prompt_cache_key".into(), Value::String(key.clone()));
    Some(format!("set prompt_cache_key={key}"))
}

fn system_prefix(body: &Value) -> String {
    if let Some(instructions) = body.get("instructions") {
        return instructions.to_string();
    }
    let key = if body.get("input").is_some() {
        "input"
    } else {
        "messages"
    };
    body.get(key)
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .filter(|m| {
            matches!(
                m.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .map(Value::to_string)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_breakpoints_when_none_present() {
        let mut body = json!({
            "tools": [{"name": "a"}],
            "system": "S",
            "messages": [
                {"role": "user", "content": "1"},
                {"role": "assistant", "content": "2"},
                {"role": "user", "content": "3"}
            ]
        });
        assert!(add_anthropic_breakpoints(&mut body).is_some());
        assert_eq!(count_breakpoints(&body), 4);
        assert!(body["messages"][2]["content"][0]["cache_control"].is_object());
        assert!(body["messages"][0]["content"][0]["cache_control"].is_object());
    }

    #[test]
    fn respects_client_breakpoints() {
        let mut body = json!({"system": [{"type": "text", "text": "S",
            "cache_control": {"type": "ephemeral"}}], "messages": []});
        assert!(add_anthropic_breakpoints(&mut body).is_none());
    }

    #[test]
    fn prompt_cache_key_is_stable_and_not_overwritten() {
        let mut a = json!({"model": "gpt-5", "messages": [{"role": "system", "content": "s"}]});
        let mut b = a.clone();
        add_prompt_cache_key(&mut a);
        add_prompt_cache_key(&mut b);
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
        let mut c = json!({"prompt_cache_key": "mine"});
        assert!(add_prompt_cache_key(&mut c).is_none());
    }
}
