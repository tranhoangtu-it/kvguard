//! Anthropic `/v1/messages` usage extraction (JSON body or SSE `message_start`/`message_delta`).

use serde_json::Value;

use super::read_u64;
use crate::monitor::Usage;

/// Merge the `usage` object carried by an Anthropic response or stream event.
///
/// `input_tokens` counts only uncached tokens, so the full prompt is
/// `input + cache_creation_input_tokens + cache_read_input_tokens`.
pub fn merge_usage(event: &Value, acc: &mut Usage) {
    let Some(usage) = event
        .get("usage")
        .or_else(|| event.pointer("/message/usage"))
    else {
        return;
    };
    if let Some(v) = read_u64(usage, "/input_tokens") {
        acc.prefilled_tokens = Some(v);
    }
    if let Some(v) = read_u64(usage, "/cache_creation_input_tokens") {
        acc.cache_write_tokens = Some(v);
    }
    if let Some(v) = read_u64(usage, "/cache_read_input_tokens") {
        acc.cached_tokens = Some(v);
    }
    if acc.prefilled_tokens.is_some() || acc.cached_tokens.is_some() {
        acc.prompt_tokens = Some(
            acc.prefilled_tokens.unwrap_or(0)
                + acc.cache_write_tokens.unwrap_or(0)
                + acc.cached_tokens.unwrap_or(0),
        );
        acc.cached_tokens.get_or_insert(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_message_start_usage() {
        let mut usage = Usage::default();
        merge_usage(
            &json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 10, "cache_creation_input_tokens": 200, "cache_read_input_tokens": 9000
            }}}),
            &mut usage,
        );
        merge_usage(
            &json!({"type": "message_delta", "usage": {"output_tokens": 42}}),
            &mut usage,
        );
        assert_eq!(usage.prompt_tokens, Some(9210));
        assert_eq!(usage.cached_tokens, Some(9000));
        assert_eq!(usage.cache_write_tokens, Some(200));
    }
}
