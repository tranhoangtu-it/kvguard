//! USD cost of cache hits/misses and local GPU prefill time.
//!
//! Prices are list prices in USD per million input tokens and are estimates: provider
//! pricing changes, and unknown model versions fall back to their family's price.

/// Per-million-token input prices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    /// Price of writing a token into the cache (Anthropic charges a premium; OpenAI does not).
    pub cache_write: f64,
    pub cache_read: f64,
}

const fn anthropic(input: f64) -> Price {
    Price {
        input,
        cache_write: input * 1.25,
        cache_read: input * 0.1,
    }
}

const fn openai(input: f64, cache_read: f64) -> Price {
    Price {
        input,
        cache_write: input,
        cache_read,
    }
}

/// Ordered (most specific first) substring table.
const PRICE_TABLE: &[(&str, Price)] = &[
    ("opus-4-1", anthropic(15.0)),
    ("opus-4-0", anthropic(15.0)),
    ("opus-4-2025", anthropic(15.0)),
    ("claude-3-opus", anthropic(15.0)),
    ("opus", anthropic(5.0)),
    ("sonnet", anthropic(3.0)),
    ("haiku-3-5", anthropic(0.8)),
    ("3-5-haiku", anthropic(0.8)),
    ("claude-3-haiku", anthropic(0.25)),
    ("haiku", anthropic(1.0)),
    ("gpt-5-nano", openai(0.05, 0.005)),
    ("gpt-5-mini", openai(0.25, 0.025)),
    ("gpt-5", openai(1.25, 0.125)),
    ("gpt-4.1-nano", openai(0.1, 0.025)),
    ("gpt-4.1-mini", openai(0.4, 0.1)),
    ("gpt-4.1", openai(2.0, 0.5)),
    ("gpt-4o-mini", openai(0.15, 0.075)),
    ("gpt-4o", openai(2.5, 1.25)),
    ("o4-mini", openai(1.1, 0.275)),
    ("o3", openai(2.0, 0.5)),
    ("gemini-2.5-pro", openai(1.25, 0.3125)),
    ("gemini-2.5-flash", openai(0.3, 0.075)),
];

/// Look up a price by model id (case-insensitive substring match).
pub fn price_for(model: &str) -> Option<Price> {
    let model = model.to_ascii_lowercase();
    PRICE_TABLE
        .iter()
        .find(|(pattern, _)| model.contains(pattern))
        .map(|(_, price)| *price)
}

/// Money lost because `invalidated_tokens` had to be (re)written instead of read from cache.
pub fn wasted_usd(price: Price, invalidated_tokens: u64) -> f64 {
    per_million(invalidated_tokens) * (price.cache_write - price.cache_read)
}

/// Money saved because `cached_tokens` were read from cache instead of billed at full price.
pub fn saved_usd(price: Price, cached_tokens: u64) -> f64 {
    per_million(cached_tokens) * (price.input - price.cache_read)
}

/// Seconds of prefill for `tokens` at `tokens_per_sec`.
pub fn prefill_seconds(tokens: u64, tokens_per_sec: f64) -> f64 {
    if tokens_per_sec <= 0.0 {
        0.0
    } else {
        tokens as f64 / tokens_per_sec
    }
}

fn per_million(tokens: u64) -> f64 {
    tokens as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_patterns_win_over_family() {
        assert_eq!(price_for("claude-opus-4-1-20250805").unwrap().input, 15.0);
        assert_eq!(price_for("claude-opus-4-5-20251101").unwrap().input, 5.0);
        assert_eq!(price_for("gpt-5-mini").unwrap().input, 0.25);
        assert!(price_for("qwen3-coder-30b").is_none());
    }

    #[test]
    fn wasted_uses_write_minus_read() {
        let p = price_for("claude-sonnet-4-5").unwrap();
        let wasted = wasted_usd(p, 1_000_000);
        assert!((wasted - (3.75 - 0.3)).abs() < 1e-9);
    }
}
