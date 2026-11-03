//! Runtime configuration: upstream routing, heal options and local-GPU defaults.

use std::path::PathBuf;

/// Reference prefill speed used to estimate wasted GPU time until real
/// `timings.prompt_ms` measurements arrive from llama-server.
pub const DEFAULT_PREFILL_TOK_PER_SEC: f64 = 850.0;

const ANTHROPIC_DEFAULT: &str = "https://api.anthropic.com";
const OPENAI_DEFAULT: &str = "https://api.openai.com";
const OLLAMA_DEFAULT: &str = "http://127.0.0.1:11434";

/// Optional heal rules that change request content beyond pure canonicalization.
#[derive(Debug, Clone, Default)]
pub struct HealOptions {
    /// Move a volatile `gitStatus:` section out of the system prompt to the newest user turn.
    pub relocate_git_status: bool,
    /// Strip `<think>…</think>` / `reasoning_content` from historical assistant turns.
    pub normalize_think_tags: bool,
}

/// Explicit backend selection; `Auto` probes the upstream once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum BackendHint {
    #[default]
    Auto,
    LlamaCpp,
    Ollama,
    Generic,
}

/// Where each API family is forwarded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstreams {
    pub anthropic: String,
    pub openai: String,
    pub ollama: String,
}

impl Upstreams {
    /// Resolve upstreams from `--target` or from the caller's original environment.
    ///
    /// `env` is a lookup function so the wrapper can pass values captured before it
    /// overrides `*_BASE_URL` for the child process.
    pub fn resolve(target: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Self {
        if let Some(target) = target {
            let base = strip_v1(target);
            return Self {
                anthropic: base.clone(),
                openai: base.clone(),
                ollama: base,
            };
        }
        let non_empty = |key: &str| env(key).filter(|v| !v.trim().is_empty());
        Self {
            anthropic: non_empty("ANTHROPIC_BASE_URL")
                .map(|v| strip_v1(&v))
                .unwrap_or_else(|| ANTHROPIC_DEFAULT.to_string()),
            openai: non_empty("OPENAI_BASE_URL")
                .map(|v| strip_v1(&v))
                .unwrap_or_else(|| OPENAI_DEFAULT.to_string()),
            ollama: non_empty("OLLAMA_HOST")
                .map(|v| with_scheme(&v))
                .unwrap_or_else(|| OLLAMA_DEFAULT.to_string()),
        }
    }

    /// Pick the upstream base URL for a request path.
    pub fn for_path(&self, path: &str) -> &str {
        if path.starts_with("/v1/messages") {
            &self.anthropic
        } else if path.starts_with("/api/") {
            &self.ollama
        } else {
            &self.openai
        }
    }
}

/// Remove trailing `/` and a trailing `/v1` so request paths (which carry `/v1/...`) can be appended.
fn strip_v1(url: &str) -> String {
    let trimmed = with_scheme(url.trim()).trim_end_matches('/').to_string();
    trimmed
        .strip_suffix("/v1")
        .map(str::to_string)
        .unwrap_or(trimmed)
}

fn with_scheme(url: &str) -> String {
    if url.contains("://") {
        url.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", url.trim_end_matches('/'))
    }
}

/// Full proxy configuration shared by the server, TUI and CI modes.
#[derive(Debug, Clone)]
pub struct Config {
    pub upstreams: Upstreams,
    pub heal_on_start: bool,
    pub heal_options: HealOptions,
    pub backend_hint: BackendHint,
    /// Override the llama-server slot count instead of probing `/slots`.
    pub slots_override: Option<u32>,
    /// Jinja chat template (raw `.jinja` or `tokenizer_config.json`) for local drift checks.
    pub chat_template: Option<PathBuf>,
    pub prefill_tok_per_sec: f64,
    /// Append every (post-heal) request body to this JSONL file for later `kvguard check`.
    pub record: Option<PathBuf>,
}

impl Config {
    pub fn new(upstreams: Upstreams) -> Self {
        Self {
            upstreams,
            heal_on_start: false,
            heal_options: HealOptions::default(),
            backend_hint: BackendHint::Auto,
            slots_override: None,
            chat_template: None,
            prefill_tok_per_sec: DEFAULT_PREFILL_TOK_PER_SEC,
            record: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_overrides_everything() {
        let ups = Upstreams::resolve(Some("http://127.0.0.1:8080/v1/"), |_| None);
        assert_eq!(ups.anthropic, "http://127.0.0.1:8080");
        assert_eq!(ups.for_path("/api/chat"), "http://127.0.0.1:8080");
    }

    #[test]
    fn env_values_are_normalized() {
        let ups = Upstreams::resolve(None, |k| match k {
            "OPENAI_BASE_URL" => Some("https://example.test/v1".into()),
            "OLLAMA_HOST" => Some("0.0.0.0:11434".into()),
            _ => None,
        });
        assert_eq!(ups.anthropic, ANTHROPIC_DEFAULT);
        assert_eq!(ups.openai, "https://example.test");
        assert_eq!(ups.ollama, "http://0.0.0.0:11434");
        assert_eq!(ups.for_path("/v1/messages"), ANTHROPIC_DEFAULT);
    }
}
