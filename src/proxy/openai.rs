//! OpenAI `/v1/chat/completions` and `/v1/responses` usage extraction.

use serde_json::Value;

use super::read_u64;
use crate::monitor::Usage;

/// Merge usage from a chat completion (chunk) or a Responses API object / `response.completed`.
pub fn merge_usage(event: &Value, acc: &mut Usage) {
    let usage = event
        .pointer("/response/usage")
        .or_else(|| event.get("usage"))
        .filter(|u| !u.is_null());
    let Some(usage) = usage else {
        return;
    };
    // Chat Completions
    if let Some(prompt) = read_u64(usage, "/prompt_tokens") {
        acc.prompt_tokens = Some(prompt);
        acc.cached_tokens =
            Some(read_u64(usage, "/prompt_tokens_details/cached_tokens").unwrap_or(0));
    }
    // Responses API
    if let Some(input) = read_u64(usage, "/input_tokens") {
        acc.prompt_tokens = Some(input);
        acc.cached_tokens =
            Some(read_u64(usage, "/input_tokens_details/cached_tokens").unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_chat_and_responses_usage() {
        let mut chat = Usage::default();
        merge_usage(
            &json!({"usage": {"prompt_tokens": 2000,
                "prompt_tokens_details": {"cached_tokens": 1920}}}),
            &mut chat,
        );
        assert_eq!(
            (chat.prompt_tokens, chat.cached_tokens),
            (Some(2000), Some(1920))
        );

        let mut responses = Usage::default();
        merge_usage(
            &json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 3000, "input_tokens_details": {"cached_tokens": 2048}}}}),
            &mut responses,
        );
        assert_eq!(responses.cached_tokens, Some(2048));
    }
}
