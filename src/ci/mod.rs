//! `kvguard check` / `kvguard ci`: replay recorded requests offline and fail the build when the
//! prompt prefix breaks anywhere other than the newly appended tail.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::canonicalizer::Healer;
use crate::config::HealOptions;
use crate::diff::{Diagnosis, analyze, jinja_verifier::JinjaVerifier};
use crate::lineage::{MatchKind, SessionTree, Snapshot};
use crate::provider::{ApiKind, Backend};

/// Options of a check run.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub inputs: Vec<String>,
    pub min_hit_rate: Option<f64>,
    pub fail_on_bust: bool,
    /// Apply heal rules before analysis (simulates `kvguard --heal` against a local backend).
    pub heal: bool,
    pub heal_options: HealOptions,
    pub api: Option<ApiKind>,
    pub chat_template: Option<PathBuf>,
}

/// One analyzed request.
#[derive(Debug, Clone)]
pub struct CheckRow {
    pub index: usize,
    pub source: String,
    pub branch: String,
    pub kind: MatchKind,
    pub heal_actions: Vec<String>,
    pub est_prompt_tokens: u64,
    pub est_reused_tokens: u64,
    /// Parent prefix size in tokens (denominator of the reuse rate); 0 for new branches.
    pub parent_prefix_tokens: u64,
    pub diagnosis: Option<Diagnosis>,
}

impl CheckRow {
    pub fn is_bust(&self) -> bool {
        self.diagnosis.is_some() || self.kind == MatchKind::Mutation
    }
}

/// Result of a check run.
#[derive(Debug, Clone)]
pub struct CheckReport {
    pub rows: Vec<CheckRow>,
}

impl CheckReport {
    pub fn busts(&self) -> usize {
        self.rows.iter().filter(|r| r.is_bust()).count()
    }

    /// Share of parent-prefix tokens that stayed reusable (1.0 when nothing had a parent).
    pub fn reuse_rate(&self) -> f64 {
        let (reused, total) = self.rows.iter().fold((0u64, 0u64), |(r, t), row| {
            (
                r + row.est_reused_tokens.min(row.parent_prefix_tokens),
                t + row.parent_prefix_tokens,
            )
        });
        if total == 0 {
            1.0
        } else {
            reused as f64 / total as f64
        }
    }

    /// Process exit code for CI.
    pub fn exit_code(&self, opts: &CheckOptions) -> u8 {
        let busted = opts.fail_on_bust && self.busts() > 0;
        let below = opts
            .min_hit_rate
            .is_some_and(|min| self.reuse_rate() < min);
        u8::from(busted || below)
    }
}

/// A request loaded from disk.
#[derive(Debug, Clone)]
struct Input {
    source: String,
    api: ApiKind,
    body: Value,
}

/// Run the check over all inputs, in order.
pub fn run_check(opts: &CheckOptions) -> Result<CheckReport> {
    let inputs = load_inputs(&opts.inputs, opts.api)?;
    if inputs.is_empty() {
        bail!("no requests found in {:?}", opts.inputs);
    }
    let jinja = opts
        .chat_template
        .as_deref()
        .map(JinjaVerifier::from_path)
        .transpose()?;
    let healer = Healer::new();
    let mut tree = SessionTree::new();
    let mut rows = Vec::with_capacity(inputs.len());

    for (index, input) in inputs.into_iter().enumerate() {
        let mut body = input.body;
        let heal_actions = if opts.heal {
            healer.apply_prefix_rules(
                input.api,
                Backend::LocalGeneric,
                &mut body,
                &opts.heal_options,
            )
        } else {
            Vec::new()
        };
        let snapshot = Snapshot::new(input.api, Arc::new(body));
        let outcome = tree.observe(snapshot.clone());
        let analysis = analyze(outcome.parent.as_ref(), &snapshot, jinja.as_ref());
        let parent_prefix_tokens = match (&analysis.diagnosis, outcome.kind) {
            (Some(d), _) => d.parent_tokens as u64,
            (None, MatchKind::NewBranch) => 0,
            (None, _) => analysis.est_reused_tokens,
        };
        rows.push(CheckRow {
            index: index + 1,
            source: input.source,
            branch: outcome.branch_label,
            kind: outcome.kind,
            heal_actions,
            est_prompt_tokens: analysis.est_prompt_tokens,
            est_reused_tokens: analysis.est_reused_tokens,
            parent_prefix_tokens,
            diagnosis: analysis.diagnosis,
        });
    }
    Ok(CheckReport { rows })
}

fn load_inputs(patterns: &[String], api_override: Option<ApiKind>) -> Result<Vec<Input>> {
    let mut inputs = Vec::new();
    for path in expand_paths(patterns)? {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let is_jsonl = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"));
        if !is_jsonl {
            let value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("{} is not valid JSON", path.display()))?;
            inputs.push(to_input(name, value, api_override));
            continue;
        }
        for (line_no, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).with_context(|| {
                format!("{}:{} is not valid JSON", path.display(), line_no + 1)
            })?;
            inputs.push(to_input(
                format!("{name}:{}", line_no + 1),
                value,
                api_override,
            ));
        }
    }
    Ok(inputs)
}

/// Accept either a bare request body or a `{"path": ..., "request": {...}}` record.
fn to_input(source: String, value: Value, api_override: Option<ApiKind>) -> Input {
    let path_api = value
        .get("path")
        .and_then(Value::as_str)
        .and_then(ApiKind::from_path);
    let body = match value.get("request") {
        Some(request) if request.is_object() => request.clone(),
        _ => value,
    };
    let api = api_override
        .or(path_api)
        .unwrap_or_else(|| ApiKind::guess_from_body(&body));
    Input { source, api, body }
}

/// Expand glob patterns ourselves (PowerShell and cmd do not), keeping argument order.
fn expand_paths(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let mut matched: Vec<PathBuf> = glob::glob(pattern)
                .with_context(|| format!("invalid glob {pattern}"))?
                .filter_map(Result::ok)
                .filter(|p| p.is_file())
                .collect();
            matched.sort();
            if matched.is_empty() {
                bail!("no files match {pattern}");
            }
            paths.extend(matched);
        } else {
            let path = Path::new(pattern).to_path_buf();
            if !path.is_file() {
                bail!("{pattern} is not a file");
            }
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Markdown report for stdout / GitHub job summaries.
pub fn render_markdown(report: &CheckReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## kvguard check — {} requests, {} bust(s), prefix reuse {:.1}%\n",
        report.rows.len(),
        report.busts(),
        report.reuse_rate() * 100.0
    );
    out.push_str(
        "| # | source | branch | verdict | reused / parent prefix (est. tokens) | root cause |\n",
    );
    out.push_str("|---|---|---|---|---|---|\n");
    for row in &report.rows {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} | {} | {} |",
            row.index,
            row.source,
            row.branch,
            verdict(row),
            reuse_cell(row),
            cause_cell(row)
        );
    }
    for row in report.rows.iter().filter(|r| r.is_bust()) {
        let Some(d) = &row.diagnosis else { continue };
        let _ = write!(
            out,
            "\n### #{} `{}` — {} at `{}`\n\n\
             Divergence at token ~#{} of ~{} (o200k estimate); ~{} tokens invalidated.\n\n\
             ```diff\n- {}\n+ {}\n```\n",
            row.index,
            row.source,
            d.kind.label(),
            d.json_path,
            d.divergence_token,
            d.parent_tokens,
            d.invalidated_tokens,
            single_line(&d.parent_excerpt),
            single_line(&d.current_excerpt),
        );
    }
    out
}

fn verdict(row: &CheckRow) -> String {
    match (&row.diagnosis, row.kind) {
        (Some(d), _) => format!("❌ BUST @ tok ~#{}", d.divergence_token),
        (None, MatchKind::Mutation) => "❌ BUST".to_string(),
        (None, MatchKind::NewBranch) => "◆ new branch".to_string(),
        (None, MatchKind::Continuation) if row.heal_actions.is_empty() => "✅ hit".to_string(),
        (None, MatchKind::Continuation) => "⚡ healed".to_string(),
    }
}

fn reuse_cell(row: &CheckRow) -> String {
    if row.kind == MatchKind::NewBranch {
        "–".to_string()
    } else {
        format!("{} / {}", row.est_reused_tokens, row.parent_prefix_tokens)
    }
}

fn cause_cell(row: &CheckRow) -> String {
    match &row.diagnosis {
        Some(d) => format!("{} at `{}`", d.kind.label(), d.json_path),
        None if !row.heal_actions.is_empty() => row.heal_actions.join(", "),
        None => String::new(),
    }
}

fn single_line(text: &str) -> String {
    text.replace(['\n', '\r'], "⏎")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_wrapper_and_path_set_api() {
        let input = to_input(
            "x".into(),
            json!({"path": "/v1/chat/completions", "request": {"messages": []}}),
            None,
        );
        assert_eq!(input.api, ApiKind::OpenAiChat);
        assert_eq!(input.body, json!({"messages": []}));
    }

    #[test]
    fn exit_code_respects_flags() {
        let report = CheckReport { rows: Vec::new() };
        let opts = CheckOptions {
            min_hit_rate: Some(0.95),
            fail_on_bust: true,
            ..CheckOptions::default()
        };
        assert_eq!(report.exit_code(&opts), 0);
    }
}
