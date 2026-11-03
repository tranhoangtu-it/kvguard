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
    }
}
