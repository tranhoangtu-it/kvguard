//! kvguard entrypoint: dispatches to wrapper, standalone TUI, attach or CI check mode.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use kvguard::cli::{Cli, Command, ProxyArgs, StatusArgs};
use kvguard::monitor::{Monitor, TurnStatus, render_status_line};
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
    if let Some(prices) = &cli.proxy.prices {
        kvguard::pricing::load_overrides(prices)?;
    }
    match cli.command {
        Some(Command::Check(args)) => {
            let opts = args.to_options();
            if let Some(prices) = &args.prices {
                kvguard::pricing::load_overrides(prices)?;
            }
            let report = ci::run_check(&opts)?;
            print!("{}", ci::render_markdown(&report, &opts));
            Ok(ExitCode::from(report.exit_code(&opts)))
        }
        Some(Command::Attach(args)) => {
            tui::attach(args.url, args.token).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Status(args)) => status(args).await,
        None if !cli.wrapped.is_empty() => wrapper::run(&cli.proxy, &cli.wrapped).await,
        None => standalone(&cli.proxy).await.map(|()| ExitCode::SUCCESS),
    }
}

/// `kvguard status`: totals of a running proxy, as a summary or a single status line.
async fn status(args: StatusArgs) -> Result<ExitCode> {
    let (url, token) = tui::resolve_target(args.url, args.token);
    match tui::DataSource::remote(&url, token).fetch().await {
        Ok(snapshot) if args.line => println!("{}", render_status_line(&snapshot)),
        Ok(snapshot) => print!("{}", wrapper::render_summary(&snapshot)),
        // A status line must never break the prompt it is embedded in.
        Err(_) if args.line => println!("kvguard off"),
        Err(err) => anyhow::bail!("no kvguard proxy at {url}: {err:#}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Proxy + full-screen TUI (or line output with `--no-tui`) until the user quits.
async fn standalone(args: &ProxyArgs) -> Result<()> {
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
    // `kvguard status` / `attach` read the control token from here.
    wrapper::write_attach_file(&url, &token)?;

    let result = if args.no_tui {
        eprintln!("kvguard: control token (x-kvguard-token / Bearer): {token}");
        headless(&monitor, &url).await
    } else {
        tui::run(tui::DataSource::Local(Arc::clone(&monitor))).await
    };
    server.abort();
    wrapper::remove_attach_file();
    eprint!("{}", wrapper::render_summary(&monitor.snapshot()));
    result
}

/// Print a line per analyzed turn until Ctrl-C, and again whenever its status changes:
/// provider usage arrives after the stream ends and can turn a HIT into PARTIAL or MISS.
async fn headless(monitor: &Monitor, url: &str) -> Result<()> {
    eprintln!("kvguard: listening on {url}");
    eprintln!("  ANTHROPIC_BASE_URL={url}  OPENAI_BASE_URL={url}/v1  OLLAMA_HOST={url}");
    let mut printed: HashMap<u64, TurnStatus> = HashMap::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            () = tokio::time::sleep(HEADLESS_POLL) => {}
        }
        for turn in monitor.snapshot().turns {
            let analyzed = turn.est_prompt_tokens.is_some() || turn.status == TurnStatus::Error;
            if !analyzed || printed.insert(turn.id, turn.status) == Some(turn.status) {
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
