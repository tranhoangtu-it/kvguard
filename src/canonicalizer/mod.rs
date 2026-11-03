//! In-flight auto-healer: semantics-preserving rewrites that keep the prompt prefix stable.
//!
//! Rules run in a fixed order. "Prefix" rules change prompt content and run before lineage
//! hashing; "routing" rules only add cache hints and run after the branch is known.

pub mod breakpoints;
pub mod jcs;
pub mod sanitizer;
pub mod slot_router;

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use xxhash_rust::xxh64::xxh64;

use crate::config::HealOptions;
use crate::lineage::block_hasher::strip_cache_control;
use crate::provider::{ApiKind, Backend};
use sanitizer::{conversation_key, is_role};

/// Cap on remembered per-session values before the maps are reset.
const MAX_REMEMBERED: usize = 10_000;

/// Stateful healer shared by all requests of a kvguard process.
#[derive(Debug, Default)]
pub struct Healer {
    /// System-prompt shape hash → timestamps pinned at first sight.
    pinned_timestamps: Mutex<HashMap<u64, Vec<String>>>,
    /// User-message hash → git status section appended to it when it was the newest turn.
    git_tails: Mutex<HashMap<u64, String>>,
}

impl Healer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rules that rewrite prompt content. Returns human-readable actions taken.
    pub fn apply_prefix_rules(
        &self,
        api: ApiKind,
        backend: Backend,
        body: &mut Value,
        opts: &HealOptions,
    ) -> Vec<String> {
        let mut actions = Vec::new();
        actions.extend(jcs::canonicalize_tools(body));
        // The official API uses the billing header for attribution; only other backends
        // (which hash it into the KV prefix) get it removed.
        if backend != Backend::AnthropicCloud {
            actions.extend(sanitizer::strip_billing_headers(api, body));
        }
        actions.extend(self.pin_timestamps(api, body));
        if opts.relocate_git_status {
            actions.extend(self.relocate_git_status(api, body));
        }
        if opts.normalize_think_tags && backend.is_local() {
            actions.extend(sanitizer::normalize_think_tags(api, body));
        }
        actions
    }

    fn pin_timestamps(&self, api: ApiKind, body: &mut Value) -> Option<String> {
        let key = system_shape_hash(api, body)?;
        let mut pinned = self.pinned_timestamps.lock().ok()?;
        if pinned.len() > MAX_REMEMBERED {
            pinned.clear();
        }
        let entry = pinned.entry(key).or_default();
        sanitizer::pin_timestamps(api, body, entry)
    }

    /// Move `gitStatus:` out of the system prompt into the newest user/tool turn, and replay
    /// previously appended sections on historical turns so the history stays byte-identical.
    ///
    /// Sections are remembered per conversation prefix (hash chain of every message up to and
    /// including the target), so identical short messages ("continue") in different positions
    /// or sessions never receive each other's git state.
    fn relocate_git_status(&self, api: ApiKind, body: &mut Value) -> Option<String> {
        let key = conversation_key(api);
        // Without a newest user/tool turn to carry the section, leave the request untouched
        // rather than dropping the git status.
        let can_carry = body.get(key)?.as_array()?.last().is_some_and(|m| {
            is_role(m, &["user", "tool"])
                && m.get("content")
                    .is_some_and(|c| c.is_string() || c.is_array())
        });
        if !can_carry {
            return None;
        }
        let section = sanitizer::take_git_status(api, body);
        let messages = body.get_mut(key)?.as_array_mut()?;
        let last_idx = messages.len().checked_sub(1)?;
        let mut tails = self.git_tails.lock().ok()?;
        if tails.len() > MAX_REMEMBERED {
            tails.clear();
        }
        let mut prefix = 0u64;
        for (i, message) in messages.iter_mut().enumerate() {
            // Chain over original (pre-append) content.
            let link = [prefix.to_le_bytes(), message_hash(message).to_le_bytes()].concat();
            prefix = xxh64(&link, 0);
            if !is_role(message, &["user", "tool"]) {
                continue;
            }
            if let Some(stored) = tails.get(&prefix) {
                sanitizer::append_to_message(message, stored, api);
            } else if i == last_idx
                && let Some(section) = section.as_ref()
            {
                tails.insert(prefix, section.clone());
                sanitizer::append_to_message(message, section, api);
            }
        }
        section.map(|_| "moved gitStatus to tail".to_string())
    }
}

/// Rules that only add cache-routing hints (no prompt content change).
pub fn apply_routing_rules(
    api: ApiKind,
    backend: Backend,
    body: &mut Value,
    slot: Option<u32>,
) -> Vec<String> {
    let mut actions = Vec::new();
    match api {
        ApiKind::AnthropicMessages if !backend.is_local() => {
            actions.extend(breakpoints::add_anthropic_breakpoints(body));
        }
        // Unknown fields can be rejected by strict gateways, so only the official API gets it.
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses if backend == Backend::OpenAiCloud => {
            actions.extend(breakpoints::add_prompt_cache_key(body));
        }
        _ => {}
    }
    if let (Backend::LlamaCpp { .. }, Some(slot)) = (backend, slot)
        && slot_router::inject_slot(body, slot)
    {
        actions.push(format!("pinned id_slot={slot}"));
    }
    actions
}

fn message_hash(message: &Value) -> u64 {
    xxh64(strip_cache_control(message).to_string().as_bytes(), 0)
}

/// Identifies one session's system prompt: model + system texts with ISO timestamps masked +
/// the opening conversation message (so concurrent sessions don't share pinned values).
fn system_shape_hash(api: ApiKind, body: &mut Value) -> Option<u64> {
    let mut shape = String::new();
    let mut has_timestamp = false;
    sanitizer::for_each_system_text(api, body, |text| {
        has_timestamp |= sanitizer::ISO_TIMESTAMP.is_match(text);
        shape.push_str(&sanitizer::ISO_TIMESTAMP.replace_all(text, "<ts>"));
        shape.push('\u{0}');
    });
    if !has_timestamp {
        return None;
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    shape.push_str(model);
    let opening = body
        .get(conversation_key(api))
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find(|m| !is_role(m, &["system", "developer"])))
        .map(message_hash)
        .unwrap_or(0);
    shape.push_str(&opening.to_string());
    Some(xxh64(shape.as_bytes(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts() -> HealOptions {
        HealOptions {
            relocate_git_status: true,
            normalize_think_tags: false,
        }
    }

    #[test]
    fn billing_header_is_kept_for_official_anthropic() {
        let healer = Healer::new();
        let original = json!({"system": "x-anthropic-billing-header: cch=1;\nYou are Claude.",
            "messages": []});
        let mut body = original.clone();
        healer.apply_prefix_rules(
            ApiKind::AnthropicMessages,
            Backend::AnthropicCloud,
            &mut body,
            &HealOptions::default(),
        );
        assert_eq!(body, original);
    }

    #[test]
    fn relocated_git_status_is_replayed_on_history() {
        let healer = Healer::new();
        let api = ApiKind::AnthropicMessages;
        let backend = Backend::LocalGeneric;
        let mut turn1 = json!({"system": "Core.\ngitStatus: M a.rs",
            "messages": [{"role": "user", "content": "hi"}]});
        healer.apply_prefix_rules(api, backend, &mut turn1, &opts());

        let mut turn2 = json!({"system": "Core.\ngitStatus: M a.rs\nM b.rs",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]});
        healer.apply_prefix_rules(api, backend, &mut turn2, &opts());

        assert_eq!(turn1["system"], turn2["system"]);
        assert_eq!(turn1["messages"][0], turn2["messages"][0]);
        assert!(
            turn2["messages"][2]["content"]
                .as_str()
                .unwrap()
                .ends_with("M b.rs")
        );
    }

    #[test]
    fn repeated_short_message_gets_fresh_git_status() {
        let healer = Healer::new();
        let api = ApiKind::AnthropicMessages;
        let backend = Backend::LocalGeneric;
        let mut turn1 = json!({"system": "Core.\ngitStatus: S1",
            "messages": [{"role": "user", "content": "continue"}]});
        healer.apply_prefix_rules(api, backend, &mut turn1, &opts());
        let mut turn2 = json!({"system": "Core.\ngitStatus: S2",
        "messages": [
            {"role": "user", "content": "continue"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "continue"}
        ]});
        healer.apply_prefix_rules(api, backend, &mut turn2, &opts());

        let text = |i: usize| {
            turn2["messages"][i]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(
            text(0).ends_with("S1"),
            "history keeps its original section"
        );
        assert!(
            text(2).ends_with("S2"),
            "newest turn gets the current section"
        );
    }

    #[test]
    fn git_status_is_kept_when_no_turn_can_carry_it() {
        let healer = Healer::new();
        let mut body = json!({"system": "Core.\ngitStatus: S1",
            "messages": [{"role": "user", "content": "q"}, {"role": "assistant", "content": "prefill"}]});
        let actions = healer.apply_prefix_rules(
            ApiKind::AnthropicMessages,
            Backend::LocalGeneric,
            &mut body,
            &opts(),
        );
        assert!(actions.is_empty());
        assert_eq!(body["system"], "Core.\ngitStatus: S1");
    }
}
