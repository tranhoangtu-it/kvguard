//! Removes per-request noise from the prompt prefix: rotating billing headers,
//! wall-clock timestamps, git status snapshots and historical `<think>` blocks.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::provider::ApiKind;

static BILLING_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*x-anthropic-billing-header:[^\n]*(\n|$)").expect("valid regex")
});
/// ISO-8601 date-times with at least second precision, and 10/13-digit Unix timestamps.
pub static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\b1[6-9]\d{8}(?:\d{3})?\b",
    )
    .expect("valid regex")
});
static GIT_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)gitStatus:.*\z").expect("valid regex"));
static THINK_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>\s*").expect("valid regex"));

/// Placeholder left in the system prompt when the git status section is relocated.
pub const GIT_STATUS_PLACEHOLDER: &str = "[GIT_STATUS_MOVED_TO_TAIL]";

/// True when the text carries a Claude Code billing header line.
pub fn has_billing_header(text: &str) -> bool {
    BILLING_HEADER.is_match(text)
}

/// Remove billing header lines.
pub fn strip_billing_header(text: &str) -> String {
    BILLING_HEADER.replace_all(text, "").into_owned()
}

/// Replace timestamps with a fixed token (used for diagnosis, not healing).
pub fn mask_timestamps(text: &str) -> String {
    TIMESTAMP.replace_all(text, "<ts>").into_owned()
}

/// Apply `f` to every system-prompt string in the request (in prompt order).
pub fn for_each_system_text(api: ApiKind, body: &mut Value, mut f: impl FnMut(&mut String)) {
    match api {
        ApiKind::AnthropicMessages => match body.get_mut("system") {
            Some(Value::String(s)) => f(s),
            Some(Value::Array(parts)) => {
                parts.iter_mut().for_each(|p| visit_text_part(p, &mut f));
            }
            _ => {}
        },
        ApiKind::OpenAiChat | ApiKind::OllamaChat | ApiKind::OpenAiResponses => {
            if let Some(Value::String(s)) = body.get_mut("instructions") {
                f(s);
            }
            let key = conversation_key(api);
            let Some(items) = body.get_mut(key).and_then(Value::as_array_mut) else {
                return;
            };
            for item in items
                .iter_mut()
                .filter(|m| is_role(m, &["system", "developer"]))
            {
                match item.get_mut("content") {
                    Some(Value::String(s)) => f(s),
                    Some(Value::Array(parts)) => {
                        parts.iter_mut().for_each(|p| visit_text_part(p, &mut f));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Name of the conversation array for an API family.
pub fn conversation_key(api: ApiKind) -> &'static str {
    if api == ApiKind::OpenAiResponses {
        "input"
    } else {
        "messages"
    }
}

fn visit_text_part(part: &mut Value, f: &mut impl FnMut(&mut String)) {
    if let Some(Value::String(s)) = part.get_mut("text") {
        f(s);
    }
}

pub fn is_role(message: &Value, roles: &[&str]) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|r| roles.contains(&r))
}

/// Strip billing headers from every system text; drops Anthropic system parts left empty.
pub fn strip_billing_headers(api: ApiKind, body: &mut Value) -> Option<String> {
    let mut removed = 0usize;
    for_each_system_text(api, body, |text| {
        if has_billing_header(text) {
            *text = strip_billing_header(text);
            removed += 1;
        }
    });
    if removed == 0 {
        return None;
    }
    if let Some(Value::Array(parts)) = body.get_mut("system") {
        parts.retain(|p| {
            p.get("text")
                .and_then(Value::as_str)
                .is_none_or(|t| !t.trim().is_empty())
        });
    }
    Some("stripped cch= billing header".to_string())
}

/// Replace every timestamp in system texts with the values pinned in `pinned`.
///
/// `pinned` holds the timestamps observed the first time this system prompt shape was seen;
/// new occurrences beyond the stored list are appended (and then kept stable).
pub fn pin_timestamps(api: ApiKind, body: &mut Value, pinned: &mut Vec<String>) -> Option<String> {
    let mut index = 0usize;
    let mut replaced = 0usize;
    for_each_system_text(api, body, |text| {
        if !TIMESTAMP.is_match(text) {
            return;
        }
        let updated = TIMESTAMP.replace_all(text, |caps: &regex::Captures| {
            let current = caps[0].to_string();
            let value = match pinned.get(index) {
                Some(stored) => stored.clone(),
                None => {
                    pinned.push(current.clone());
                    current.clone()
                }
            };
            if value != current {
                replaced += 1;
            }
            index += 1;
            value
        });
        *text = updated.into_owned();
    });
    (replaced > 0).then(|| format!("pinned {replaced} timestamp(s)"))
}

/// Remove the `gitStatus:` section from system texts. Returns the removed section.
pub fn take_git_status(api: ApiKind, body: &mut Value) -> Option<String> {
    let mut taken = None;
    for_each_system_text(api, body, |text| {
        if taken.is_some() {
            return;
        }
        if let Some(m) = GIT_STATUS.find(text) {
            taken = Some(m.as_str().trim_end().to_string());
            text.replace_range(m.range(), GIT_STATUS_PLACEHOLDER);
        }
    });
    taken
}

/// Append `extra` text to a message's content (string or content-part array).
pub fn append_to_message(message: &mut Value, extra: &str, api: ApiKind) {
    match message.get_mut("content") {
        Some(Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(extra);
        }
        Some(Value::Array(parts)) => {
            let part_type = if api == ApiKind::OpenAiResponses {
                "input_text"
            } else {
                "text"
            };
            parts.push(serde_json::json!({"type": part_type, "text": extra}));
        }
        _ => {}
    }
}

/// Remove `<think>` blocks and reasoning fields from historical assistant turns.
pub fn normalize_think_tags(api: ApiKind, body: &mut Value) -> Option<String> {
    let messages = body.get_mut(conversation_key(api))?.as_array_mut()?;
    let mut changed = 0usize;
    for message in messages.iter_mut().filter(|m| is_role(m, &["assistant"])) {
        let before = message.to_string();
        if let Some(map) = message.as_object_mut() {
            map.remove("reasoning_content");
            map.remove("reasoning");
            map.remove("thinking");
        }
        match message.get_mut("content") {
            Some(Value::String(s)) => *s = THINK_BLOCK.replace_all(s, "").into_owned(),
            Some(Value::Array(parts)) => {
                for part in parts.iter_mut() {
                    if let Some(Value::String(t)) = part.get_mut("text") {
                        *t = THINK_BLOCK.replace_all(t, "").into_owned();
                    }
                }
            }
            _ => {}
        }
        if message.to_string() != before {
            changed += 1;
        }
    }
    (changed > 0).then(|| format!("normalized <think> in {changed} turn(s)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_billing_header_and_empty_parts() {
        let mut body = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1; cch=abc12;"},
            {"type": "text", "text": "You are Claude."}
        ]});
        let action = strip_billing_headers(ApiKind::AnthropicMessages, &mut body);
        assert!(action.is_some());
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pins_timestamps_across_requests() {
        let mut pinned = Vec::new();
        let mut a = json!({"system": "Now: 2026-11-03T22:40:19.123Z"});
        assert!(pin_timestamps(ApiKind::AnthropicMessages, &mut a, &mut pinned).is_none());
        let mut b = json!({"system": "Now: 2026-11-03T22:41:07.001Z"});
        assert!(pin_timestamps(ApiKind::AnthropicMessages, &mut b, &mut pinned).is_some());
        assert_eq!(a, b);
    }

    #[test]
    fn removes_think_blocks_from_assistant_history() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "<think>hmm</think>\nanswer", "reasoning_content": "hmm"}
        ]});
        assert!(normalize_think_tags(ApiKind::OpenAiChat, &mut body).is_some());
        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": "answer"})
        );
    }
}
