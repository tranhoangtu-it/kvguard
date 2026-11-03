//! Byte/token divergence between two prompt strings.
//!
//! Token counts use OpenAI's `o200k_base` vocabulary. They are exact for GPT-4o/GPT-5-family
//! models and an estimate for every other model (Claude's tokenizer is not public; local
//! models use their own vocabularies). Callers label them as estimates where relevant.

use serde::{Deserialize, Serialize};
use tiktoken_rs::o200k_base_singleton;

const EXCERPT_BEFORE: usize = 80;
const EXCERPT_AFTER: usize = 120;
const WINDOW_TOKENS_BEFORE: usize = 4;
const WINDOW_TOKENS_AFTER: usize = 8;

/// Where two prompt strings stop sharing a prefix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenDivergence {
    pub divergence_byte: usize,
    pub divergence_token: usize,
    pub parent_tokens: usize,
    pub parent_excerpt: String,
    pub current_excerpt: String,
    /// Decoded tokens around the divergence point (parent side).
    pub parent_window: Vec<String>,
    /// Decoded tokens around the divergence point (current side).
    pub current_window: Vec<String>,
}

/// Count tokens of `text` (o200k estimate).
pub fn count_tokens(text: &str) -> usize {
    o200k_base_singleton().encode_ordinary(text).len()
}

/// Length in bytes of the longest common prefix, snapped to a char boundary of both strings.
pub fn common_prefix_bytes(a: &str, b: &str) -> usize {
    let raw = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count();
    floor_char_boundary(a, raw)
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Compute the divergence between a parent prompt and the current prompt.
pub fn diverge(parent: &str, current: &str) -> TokenDivergence {
    let byte = common_prefix_bytes(parent, current);
    TokenDivergence {
        divergence_byte: byte,
        divergence_token: count_tokens(&parent[..byte]),
        parent_tokens: count_tokens(parent),
        parent_excerpt: excerpt(parent, byte),
        current_excerpt: excerpt(current, byte),
        parent_window: token_window(parent, byte),
        current_window: token_window(current, byte),
    }
}

/// Text around `byte`, with a `⟦` marker at the divergence point.
pub fn excerpt(text: &str, byte: usize) -> String {
    let byte = floor_char_boundary(text, byte);
    let start = floor_char_boundary(text, byte.saturating_sub(EXCERPT_BEFORE));
    let end = ceil_char_boundary(text, byte + EXCERPT_AFTER);
    format!("{}⟦{}", &text[start..byte], &text[byte..end])
}

/// Decoded tokens surrounding the divergence point; the first token after the cut is `»`-marked.
fn token_window(text: &str, byte: usize) -> Vec<String> {
    let bpe = o200k_base_singleton();
    let byte = floor_char_boundary(text, byte);
    let start = floor_char_boundary(text, byte.saturating_sub(EXCERPT_BEFORE));
    let end = ceil_char_boundary(text, byte + EXCERPT_AFTER);
    let before = bpe.encode_ordinary(&text[start..byte]);
    let after = bpe.encode_ordinary(&text[byte..end]);
    let decode = |t: u32| {
        bpe.decode_bytes(&[t])
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let head = before
        .iter()
        .skip(before.len().saturating_sub(WINDOW_TOKENS_BEFORE))
        .map(|&t| decode(t));
    let tail = after
        .iter()
        .take(WINDOW_TOKENS_AFTER)
        .enumerate()
        .map(|(i, &t)| {
            let piece = decode(t);
            if i == 0 { format!("»{piece}") } else { piece }
        });
    head.chain(tail).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_divergence_and_marks_excerpt() {
        let parent = "You are a helpful assistant. Session id 11111 and more text.";
        let current = "You are a helpful assistant. Session id 22222 and more text.";
        let d = diverge(parent, current);
        assert_eq!(
            &parent[..d.divergence_byte],
            "You are a helpful assistant. Session id "
        );
        assert!(d.divergence_token > 0 && d.divergence_token < d.parent_tokens);
        assert!(d.parent_excerpt.contains("⟦11111"));
        assert!(d.current_window.iter().any(|t| t.starts_with('»')));
    }

    #[test]
    fn prefix_snaps_to_char_boundary() {
        assert_eq!(common_prefix_bytes("aé", "aè"), 1);
    }
}
