//! Command-line interface.
//!
//! ```text
//! kvguard [OPTIONS] -- <COMMAND> [ARGS...]   wrap a command (sidecar proxy, summary on exit)
//! kvguard [OPTIONS]                          standalone proxy + live TUI
//! kvguard check <FILES>...                   offline CI gate (alias: ci)
//! kvguard attach                             live TUI for a running wrapper
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::ci::CheckOptions;
use crate::config::{BackendHint, Config, DEFAULT_PREFILL_TOK_PER_SEC, HealOptions, Upstreams};
use crate::provider::ApiKind;

#[derive(Debug, Parser)]
#[command(
    name = "kvguard",
    version,
    about = "Local proxy & TUI that catches prompt/KV cache prefix misses and heals requests.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub proxy: ProxyArgs,

    /// Command to run behind the proxy (everything after `--`).
    #[arg(last = true, value_name = "COMMAND")]
    pub wrapped: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Replay request fixtures / recorded traces and fail on prefix busts.
    #[command(alias = "ci")]
    Check(CheckArgs),
    /// Open the live TUI for a kvguard proxy started in wrapper mode.
    Attach(AttachArgs),
    /// Print a running proxy's totals (`--line` for shell prompts and editor status lines).
    Status(StatusArgs),
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Proxy URL (default: the most recent wrapper, else http://127.0.0.1:4141).
    #[arg(long)]
    pub url: Option<String>,
    /// Control token (default: read from the wrapper's attach file).
    #[arg(long, env = "KVGUARD_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// One compact line; prints `kvguard off` (exit 0) when no proxy is running.
    #[arg(long)]
    pub line: bool,
}

/// Heal-rule flags shared by the proxy and `check`.
#[derive(Debug, Clone, Args)]
pub struct HealArgs {
    /// Pin ISO-8601 times in the system prompt to the session start (the model then sees
    /// the start time, not the current time).
    #[arg(long)]
    pub pin_timestamps: bool,
    /// Move a volatile `gitStatus:` section from the system prompt to the newest turn.
    #[arg(long)]
    pub relocate_git_status: bool,
    /// Strip `<think>` blocks from assistant turns before the latest user message
    /// (local Qwen/DeepSeek models).
    #[arg(long)]
    pub normalize_think_tags: bool,
}

impl HealArgs {
    fn options(&self) -> HealOptions {
        HealOptions {
            pin_timestamps: self.pin_timestamps,
            relocate_git_status: self.relocate_git_status,
            normalize_think_tags: self.normalize_think_tags,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct ProxyArgs {
    /// Apply the safe heal rules (tool ordering, schema key order, cache hints, slot routing;
    /// billing header removed on non-Anthropic backends). Toggle live with `h` in the TUI.
    #[arg(long)]
    pub heal: bool,
    /// Token required by the control endpoints (`/__kvguard/*`); random when unset.
    #[arg(long, env = "KVGUARD_TOKEN", hide_env_values = true)]
    pub control_token: Option<String>,
    /// JSON file overriding list prices: {"model-substring": {"input": 3.0,
    /// "cache_write": 3.75, "cache_read": 0.3}} in USD per million tokens.
    #[arg(long)]
    pub prices: Option<PathBuf>,
    /// Listen port on 127.0.0.1 (default: 4141, or a free port when 4141 is busy).
    #[arg(long, env = "KVGUARD_PORT")]
    pub port: Option<u16>,
    /// Forward every API family to this upstream (e.g. http://127.0.0.1:8080 for llama-server).
    #[arg(long, env = "KVGUARD_TARGET")]
    pub target: Option<String>,
    /// Upstream server type (auto probes llama-server's /props and /slots).
    #[arg(long, value_enum, default_value_t = BackendHint::Auto)]
    pub backend: BackendHint,
    /// llama-server parallel slot count, overriding the probe.
    #[arg(long)]
    pub slots: Option<u32>,
    /// Chat template (.jinja or tokenizer_config.json) to detect template drift on local models.
    #[arg(long)]
    pub chat_template: Option<PathBuf>,
    /// Reference prefill speed (tokens/s) until llama-server timings calibrate it.
    #[arg(long, default_value_t = DEFAULT_PREFILL_TOK_PER_SEC)]
    pub prefill_tps: f64,
    /// Append every forwarded request body to this JSONL file (input for `kvguard check`).
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// Report PARTIAL when the provider serves less than this share of what the previous
    /// turn cached (0.0–1.0).
    #[arg(long, default_value_t = crate::monitor::DEFAULT_PARTIAL_THRESHOLD)]
    pub partial_threshold: f64,
    /// Standalone mode: print one line per turn instead of the full-screen TUI.
    #[arg(long)]
    pub no_tui: bool,
    #[command(flatten)]
    pub heal_rules: HealArgs,
}

impl ProxyArgs {
    /// Build the runtime configuration; `env` reads the caller's original environment.
    pub fn to_config(&self, env: impl Fn(&str) -> Option<String>) -> Config {
        let mut config = Config::new(Upstreams::resolve(self.target.as_deref(), env));
        config.heal_on_start = self.heal;
        config.heal_options = self.heal_rules.options();
        config.backend_hint = self.backend;
        config.slots_override = self.slots;
        config.chat_template = self.chat_template.clone();
        config.prefill_tok_per_sec = self.prefill_tps;
        config.record = self.record.clone();
        config.partial_threshold = self.partial_threshold;
        config.control_token = self.control_token.clone();
        config
    }
}

#[derive(Debug, Clone, Args)]
pub struct CheckArgs {
    /// Request files: `.json` (one request) or `.jsonl` (one per line); globs are expanded.
    #[arg(required = true, value_name = "FILES")]
    pub files: Vec<String>,
    /// Fail when the estimated prefix reuse rate drops below this value (0.0–1.0).
    #[arg(long)]
    pub min_hit_rate: Option<f64>,
    /// Fail when any request breaks its parent's prefix.
    #[arg(long)]
    pub fail_on_bust: bool,
    /// Apply heal rules before analysis.
    #[arg(long)]
    pub heal: bool,
    /// Force the API format instead of inferring it from each request.
    #[arg(long, value_enum)]
    pub api: Option<ApiKind>,
    /// Chat template to verify rendered prompts for local models.
    #[arg(long)]
    pub chat_template: Option<PathBuf>,
    /// JSON file overriding list prices (same format as the proxy's `--prices`).
    #[arg(long)]
    pub prices: Option<PathBuf>,
    #[command(flatten)]
    pub heal_rules: HealArgs,
}

impl CheckArgs {
    pub fn to_options(&self) -> CheckOptions {
        CheckOptions {
            inputs: self.files.clone(),
            min_hit_rate: self.min_hit_rate,
            fail_on_bust: self.fail_on_bust,
            heal: self.heal,
            heal_options: self.heal_rules.options(),
            api: self.api,
            chat_template: self.chat_template.clone(),
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct AttachArgs {
    /// Proxy URL (default: the most recent wrapper, else http://127.0.0.1:4141).
    #[arg(long)]
    pub url: Option<String>,
    /// Control token (default: read from the wrapper's attach file).
    #[arg(long, env = "KVGUARD_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapper_invocation() {
        let cli = Cli::try_parse_from([
            "kvguard",
            "--heal",
            "--target",
            "http://127.0.0.1:8080",
            "--",
            "opencode",
            "--model",
            "x",
        ])
        .expect("parse");
        assert!(cli.proxy.heal);
        assert_eq!(cli.wrapped, ["opencode", "--model", "x"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_ci_alias() {
        let cli = Cli::try_parse_from([
            "kvguard",
            "ci",
            "a.json",
            "--min-hit-rate",
            "0.95",
            "--fail-on-bust",
        ])
        .expect("parse");
        let Some(Command::Check(args)) = cli.command else {
            panic!("expected check");
        };
        assert_eq!(args.min_hit_rate, Some(0.95));
        assert!(args.fail_on_bust);
    }
}
