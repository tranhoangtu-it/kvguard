//! Dual diff engine: JSON-AST path diff plus token / chat-template divergence.

pub mod ast_diff;
pub mod jinja_verifier;
pub mod token_diff;

use serde::{Deserialize, Serialize};

use crate::lineage::{Snapshot, prompt_text};
use jinja_verifier::JinjaVerifier;

/// Why the prompt prefix broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationKind {
    ToolOrderShuffled,
    SchemaKeyOrderChanged,
    ToolSetChanged,
    ToolDefinitionChanged,
    VolatileSystemHeader,
    DynamicTimestampInPrefix,
    SystemPromptChanged,
    HistoricalMessageMutated,
    JinjaTemplateDrift,
}

impl MutationKind {
    /// Short human label used in the TUI and CI report.
    pub fn label(self) -> &'static str {
        match self {
            Self::ToolOrderShuffled => "tools[] order shuffled",
            Self::SchemaKeyOrderChanged => "tool schema key order changed",
            Self::ToolSetChanged => "tool set changed",
            Self::ToolDefinitionChanged => "tool definition changed",
            Self::VolatileSystemHeader => "volatile header/id in system",
            Self::DynamicTimestampInPrefix => "timestamp in prefix",
            Self::SystemPromptChanged => "system prompt changed",
            Self::HistoricalMessageMutated => "history message mutated",
            Self::JinjaTemplateDrift => "chat template drift",
        }
    }
}

/// Full diagnosis of a cache bust between a parent turn and the current turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnosis {
    pub kind: MutationKind,
    pub json_path: String,
    pub old_value: String,
    pub new_value: String,
    pub divergence_byte: usize,
    /// Token index of the first divergent token (o200k estimate).
    pub divergence_token: usize,
    /// Prompt tokens of the parent turn (o200k estimate).
    pub parent_tokens: usize,
    /// Parent prefix tokens that can no longer be read from cache.
    pub invalidated_tokens: usize,
    pub parent_excerpt: String,
    pub current_excerpt: String,
    pub parent_window: Vec<String>,
    pub current_window: Vec<String>,
    /// Token positions come from the inference server's own template + tokenizer.
    #[serde(default)]
    pub exact_tokens: bool,
}

/// Token differences at the very end of the parent prompt that only come from BPE merges
/// across the boundary (e.g. `"\n"` joining the next word); the server re-evaluates just
/// these few tokens, so they are not reported as drift.
pub const BOUNDARY_TOKEN_TOLERANCE: usize = 4;

/// Re-anchor a diagnosis on exact server token ids, or report template drift that only the
/// server-rendered prompt reveals (JSON prefix intact, rendered tokens diverge).
pub fn apply_exact_tokens(
    diagnosis: Option<Diagnosis>,
    parent: (&str, &[u64]),
    current: (&str, &[u64]),
) -> Option<Diagnosis> {
    let (parent_text, parent_tokens) = parent;
    let (current_text, current_tokens) = current;
    let common = parent_tokens
        .iter()
        .zip(current_tokens)
        .take_while(|(a, b)| a == b)
        .count();
    let parent_len = parent_tokens.len();
    let with_exact = |mut d: Diagnosis| {
        d.divergence_token = common;
        d.parent_tokens = parent_len;
        d.invalidated_tokens = parent_len.saturating_sub(common);
        d.exact_tokens = true;
        d
    };
    match diagnosis {
        Some(d) => Some(with_exact(d)),
        None if parent_len.saturating_sub(common) > BOUNDARY_TOKEN_TOLERANCE => {
            let tokens = token_diff::diverge(parent_text, current_text);
            let (old_value, new_value) = (
                tokens.parent_excerpt.clone(),
                tokens.current_excerpt.clone(),
            );
            Some(with_exact(build(
                MutationKind::JinjaTemplateDrift,
                "chat_template(messages)".to_string(),
                old_value,
                new_value,
                tokens,
            )))
        }
        None => None,
    }
}

/// Diagnose a broken prefix. Runs the JSON diff first; when the JSON prefix is intact and a
/// chat template is configured, checks the rendered template instead.
pub fn diagnose(
    parent: &Snapshot,
    current: &Snapshot,
    jinja: Option<&JinjaVerifier>,
) -> Option<Diagnosis> {
    if let Some(ast) = ast_diff::first_divergence(parent, current) {
        let tokens =
            token_diff::diverge(&prompt_text(&parent.blocks), &prompt_text(&current.blocks));
        return Some(build(
            ast.kind,
            ast.json_path,
            ast.old_snippet,
            ast.new_snippet,
            tokens,
        ));
    }
    let drift = jinja?
        .check(parent.api, &parent.body, &current.body)
        .ok()??;
    let tokens = token_diff::diverge(&drift.parent_rendered, &drift.current_rendered);
    let old_value = tokens.parent_excerpt.clone();
    let new_value = tokens.current_excerpt.clone();
    Some(build(
        MutationKind::JinjaTemplateDrift,
        "chat_template(messages)".to_string(),
        old_value,
        new_value,
        tokens,
    ))
}

/// Token-level result for one turn: estimated sizes plus a diagnosis when the prefix broke.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub diagnosis: Option<Diagnosis>,
    pub est_prompt_tokens: u64,
    pub est_reused_tokens: u64,
}

/// Analyze a turn against its parent (if any). CPU-bound: run off the async runtime.
pub fn analyze(
    parent: Option<&Snapshot>,
    current: &Snapshot,
    jinja: Option<&JinjaVerifier>,
) -> Analysis {
    let current_text = prompt_text(&current.blocks);
    let Some(parent) = parent else {
        return Analysis {
            diagnosis: None,
            est_prompt_tokens: token_diff::count_tokens(&current_text) as u64,
            est_reused_tokens: 0,
        };
    };
    let diagnosis = diagnose(parent, current, jinja);
    let parent_text = prompt_text(&parent.blocks);
    let common = token_diff::common_prefix_bytes(&parent_text, &current_text);
    let reused = match &diagnosis {
        Some(d) => d.divergence_token,
        None => token_diff::count_tokens(&current_text[..common]),
    };
    let est_prompt_tokens = (token_diff::count_tokens(&current_text[..common])
        + token_diff::count_tokens(&current_text[common..])) as u64;
    Analysis {
        est_reused_tokens: (reused as u64).min(est_prompt_tokens),
        est_prompt_tokens,
        diagnosis,
    }
}

fn build(
    kind: MutationKind,
    json_path: String,
    old_value: String,
    new_value: String,
    tokens: token_diff::TokenDivergence,
) -> Diagnosis {
    Diagnosis {
        kind,
        json_path,
        old_value,
        new_value,
        divergence_byte: tokens.divergence_byte,
        divergence_token: tokens.divergence_token,
        parent_tokens: tokens.parent_tokens,
        invalidated_tokens: tokens.parent_tokens.saturating_sub(tokens.divergence_token),
        parent_excerpt: tokens.parent_excerpt,
        current_excerpt: tokens.current_excerpt,
        parent_window: tokens.parent_window,
        current_window: tokens.current_window,
        exact_tokens: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tokens_ignore_boundary_merges_but_catch_drift() {
        let parent: Vec<u64> = (0..100).collect();
        let mut boundary = parent[..98].to_vec();
        boundary.extend([500, 501, 502]);
        assert!(apply_exact_tokens(None, ("p", &parent), ("p2", &boundary)).is_none());

        let mut drifted = parent[..40].to_vec();
        drifted.extend(900..960);
        let d =
            apply_exact_tokens(None, ("abc<think>x", &parent), ("abcx", &drifted)).expect("drift");
        assert_eq!(d.kind, MutationKind::JinjaTemplateDrift);
        assert_eq!(
            (d.divergence_token, d.parent_tokens, d.invalidated_tokens),
            (40, 100, 60)
        );
        assert!(d.exact_tokens);
    }
}
