//! Google Gemini `generateContent` / `streamGenerateContent` usage extraction.

use serde_json::Value;

use super::read_u64;
use crate::monitor::Usage;

/// Merge `usageMetadata` (or `usage_metadata`) carried by a Gemini JSON response or SSE chunk.
pub fn merge_usage(event: &Value, acc: &mut Usage) {
    let usage = event
        .get("usageMetadata")
        .or_else(|| event.get("usage_metadata"))
        .filter(|u| !u.is_null());
    let Some(usage) = usage else {
        return;
    };
    if let Some(prompt) =
        read_u64(usage, "/promptTokenCount").or_else(|| read_u64(usage, "/prompt_token_count"))
    {
        acc.prompt_tokens = Some(prompt);
        acc.cached_tokens = Some(
            read_u64(usage, "/cachedContentTokenCount")
                .or_else(|| read_u64(usage, "/cached_content_token_count"))
                .unwrap_or(0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_gemini_usage_metadata() {
        let mut usage = Usage::default();
        merge_usage(
            &json!({
                "usageMetadata": {
                    "promptTokenCount": 4096,
                    "cachedContentTokenCount": 3072,
                    "candidatesTokenCount": 64
                }
            }),
            &mut usage,
        );
        assert_eq!(usage.prompt_tokens, Some(4096));
        assert_eq!(usage.cached_tokens, Some(3072));
    }
}
