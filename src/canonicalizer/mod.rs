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
    fn relocate_git_status(&self, api: ApiKind, body: &mut Value) -> Option<String> {
        let section = sanitizer::take_git_status(api, body);
        let messages = body.get_mut(conversation_key(api))?.as_array_mut()?;
        let last_idx = messages.len().checked_sub(1)?;
        let mut tails = self.git_tails.lock().ok()?;
        if tails.len() > MAX_REMEMBERED {
            tails.clear();
        }
        for (i, message) in messages.iter_mut().enumerate() {
            if !is_role(message, &["user", "tool"]) {
                continue;
            }
            let hash = message_hash(message);
            if let Some(stored) = tails.get(&hash) {
                sanitizer::append_to_message(message, stored, api);
            } else if let (true, Some(section)) = (i == last_idx, section.as_ref()) {
                tails.insert(hash, section.clone());
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
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses if backend == Backend::HostedCloud => {
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

/// Hash of model + system texts with timestamps masked: identifies "the same system prompt".
fn system_shape_hash(api: ApiKind, body: &mut Value) -> Option<u64> {
    let mut shape = String::new();
    let mut has_timestamp = false;
    sanitizer::for_each_system_text(api, body, |text| {
        has_timestamp |= sanitizer::TIMESTAMP.is_match(text);
        shape.push_str(&sanitizer::mask_timestamps(text));
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
}
