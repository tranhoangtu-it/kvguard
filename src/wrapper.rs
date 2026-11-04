//! Wrapper mode: run a command behind the proxy, then print a summary.
//!
//! The wrapped tool usually owns the terminal (Claude Code, OpenCode, Aider…), so kvguard
//! stays silent while it runs; `kvguard attach` opens the live TUI from another terminal.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::ProxyArgs;
use crate::monitor::{Monitor, StateSnapshot, TurnStatus};
use crate::proxy::{self, AppState};
use crate::tui::format::{fmt_duration, fmt_tokens};

/// Busts listed individually in the exit summary.
const SUMMARY_BUST_LINES: usize = 10;

/// Discovery record for `kvguard attach` / `kvguard status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachInfo {
    pub url: String,
    pub pid: u32,
    /// Control-endpoint token of that proxy.
    #[serde(default)]
    pub token: Option<String>,
}

/// Per-user runtime directory: `$XDG_RUNTIME_DIR/kvguard`, else `<tmp>/kvguard-<user>` on
/// Unix (shared `/tmp` must not let other users plant the file); `%TEMP%\kvguard` on Windows,
/// which is already per-user.
fn runtime_dir() -> PathBuf {
    if cfg!(windows) {
        return std::env::temp_dir().join("kvguard");
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("kvguard");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir().join(format!("kvguard-{user}"))
}

pub fn attach_file_path() -> PathBuf {
    runtime_dir().join("attach.json")
}

/// The most recent wrapper's URL and token, if any.
pub fn read_attach() -> Option<AttachInfo> {
    let raw = std::fs::read_to_string(attach_file_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_attach_file(url: &str, token: &str) -> Result<()> {
    let path = attach_file_path();
    if let Some(dir) = path.parent() {
        crate::secure_file::create_private_dir(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let info = AttachInfo {
        url: url.to_string(),
        pid: std::process::id(),
        token: Some(token.to_string()),
    };
    let mut file = crate::secure_file::create_private(&path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(&info)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn remove_attach_file() {
    let path = attach_file_path();
    let is_ours = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<AttachInfo>(&raw).ok())
        .is_some_and(|info| info.pid == std::process::id());
    if is_ours {
        // Best effort: a stale file only makes `attach` fall back to the default port.
        let _ = std::fs::remove_file(path);
    }
}

/// Run `command` with base-URL variables pointing at the proxy. Returns the child's exit code.
pub async fn run(args: &ProxyArgs, command: &[String]) -> Result<ExitCode> {
    let Some((program, program_args)) = command.split_first() else {
        bail!("no command given after `--`");
    };
    let config = args.to_config(|key| std::env::var(key).ok());
    let monitor = Arc::new(Monitor::new(
        config.heal_on_start,
        config.prefill_tok_per_sec,
    ));
    monitor.set_partial_threshold(config.partial_threshold);
    let state = AppState::new(config, Arc::clone(&monitor))?;
    let token = state.control_token().to_string();
    let listener = proxy::bind(args.port).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(proxy::serve(listener, state));
    write_attach_file(&url, &token)?;

    eprintln!(
        "kvguard: proxy {url} (auto-heal {}) — run `kvguard attach` in another terminal for the live TUI",
        if args.heal { "on" } else { "off" }
    );

    let resolved = resolve_program(program);
    let mut child = tokio::process::Command::new(&resolved)
        .args(program_args)
        .env("ANTHROPIC_BASE_URL", &url)
        .env("OPENAI_BASE_URL", format!("{url}/v1"))
        .env("OLLAMA_HOST", &url)
        .spawn()
        .with_context(|| format!("failed to start `{program}`"))?;

    // Ctrl-C reaches the child through the shared console; keep waiting for it to exit.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.context("failed to wait for child")?,
            _ = tokio::signal::ctrl_c() => {}
        }
    };

    server.abort();
    remove_attach_file();
    eprint!("{}", render_summary(&monitor.snapshot()));
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

/// Locate an executable the way a shell would. On Windows this resolves `claude` to
/// `claude.cmd` via `PATHEXT`, which `CreateProcess` does not do by itself.
fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    if !cfg!(windows) || path.extension().is_some() || path.components().count() > 1 {
        return path.to_path_buf();
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .flat_map(|dir| {
            exts.split(';')
                .filter(|e| !e.is_empty())
                .map(move |ext| dir.join(format!("{program}{}", ext.to_ascii_lowercase())))
        })
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| path.to_path_buf())
}

/// Plain-text session summary printed when the wrapped command exits.
pub fn render_summary(snapshot: &StateSnapshot) -> String {
    let totals = &snapshot.totals;
    let hit = totals
        .hit_rate()
        .map_or_else(|| "n/a".to_string(), |r| format!("{:.1}%", r * 100.0));
    let mut out = format!(
        "\nkvguard summary: {} turn(s) · cache hit {hit} · {} tok reused · saved ${:.2} · wasted ${:.2}",
        totals.turns,
        fmt_tokens(totals.tokens_saved()),
        totals.saved_usd,
        totals.wasted_usd,
    );
    if totals.gpu_saved_s > 0.0 || totals.gpu_wasted_s > 0.0 {
        out.push_str(&format!(
            " · GPU prefill saved {} / wasted {}",
            fmt_duration(totals.gpu_saved_s),
            fmt_duration(totals.gpu_wasted_s)
        ));
    }
    out.push_str(&format!(
        " · {} bust(s) · {} partial · {} miss\n",
        totals.busts, totals.partials, totals.misses
    ));
    let busts = snapshot
        .turns
        .iter()
        .filter(|t| t.status == TurnStatus::Bust)
        .take(SUMMARY_BUST_LINES);
    for turn in busts {
        match &turn.diagnosis {
            Some(d) => out.push_str(&format!(
                "  #{:02} {} BUST @ tok ~{} — {} ({})\n",
                turn.id,
                turn.branch,
                d.divergence_token,
                d.kind.label(),
                d.json_path
            )),
            None => out.push_str(&format!("  #{:02} {} BUST\n", turn.id, turn.branch)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_handles_empty_session() {
        let text = render_summary(&StateSnapshot::default());
        assert!(text.contains("0 turn(s)"));
        assert!(text.contains("cache hit n/a"));
    }

    #[test]
    fn paths_with_extension_are_not_rewritten() {
        assert_eq!(resolve_program("tool.exe"), PathBuf::from("tool.exe"));
    }
}
