//! kvguard entrypoint: dispatches to wrapper, standalone TUI, attach or CI check mode.

use std::collections::HashSet;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use kvguard::cli::{Cli, Command, ProxyArgs};
use kvguard::monitor::{Monitor, TurnStatus};
use kvguard::proxy::{self, AppState};
use kvguard::{ci, tui, wrapper};

/// Exit code for kvguard's own failures (distinct from a wrapped command's codes and CI's 1).
const EXIT_INTERNAL_ERROR: u8 = 2;
const HEADLESS_POLL: Duration = Duration::from_millis(300);

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("kvguard: {err:#}");
            ExitCode::from(EXIT_INTERNAL_ERROR)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Some(Command::Check(args)) => {
            let opts = args.to_options();
            let report = ci::run_check(&opts)?;
            print!("{}", ci::render_markdown(&report));
            Ok(ExitCode::from(report.exit_code(&opts)))
        }
        Some(Command::Attach(args)) => {
            tui::attach(args.url).await?;
            Ok(ExitCode::SUCCESS)
        }
        None if !cli.wrapped.is_empty() => wrapper::run(&cli.proxy, &cli.wrapped).await,
        None => standalone(&cli.proxy).await.map(|()| ExitCode::SUCCESS),
    }
}

/// Proxy + full-screen TUI (or line output with `--no-tui`) until the user quits.
async fn standalone(args: &ProxyArgs) -> Result<()> {
    let config = args.to_config(|key| std::env::var(key).ok());
    let monitor = Arc::new(Monitor::new(
        config.heal_on_start,
        config.prefill_tok_per_sec,
    ));
    let state = AppState::new(config, Arc::clone(&monitor))?;
    let listener = proxy::bind(args.port).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(proxy::serve(listener, state));

    let result = if args.no_tui {
        headless(&monitor, &url).await
    } else {
        tui::run(tui::DataSource::Local(Arc::clone(&monitor))).await
    };
    server.abort();
    eprint!("{}", wrapper::render_summary(&monitor.snapshot()));
    result
}

/// Print one line per analyzed turn until Ctrl-C.
async fn headless(monitor: &Monitor, url: &str) -> Result<()> {
    eprintln!("kvguard: listening on {url}");
    eprintln!("  ANTHROPIC_BASE_URL={url}  OPENAI_BASE_URL={url}/v1  OLLAMA_HOST={url}");
    let mut printed = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            () = tokio::time::sleep(HEADLESS_POLL) => {}
        }
        for turn in monitor.snapshot().turns {
            let analyzed = turn.est_prompt_tokens.is_some() || turn.status == TurnStatus::Error;
            if !analyzed || !printed.insert(turn.id) {
                continue;
            }
            let hit = turn
                .hit_ratio()
                .map_or_else(|| "  –".to_string(), |r| format!("{:>3.0}%", r * 100.0));
            let slot = turn
                .slot
                .map(|s| format!(" [slot {s}]"))
                .unwrap_or_default();
            let (cause, _) = tui::ui::cause(&turn);
            println!(
                "#{:02} {}{slot} {} {hit} {cause}",
                turn.id, turn.branch, turn.model
            );
        }
    }
}
