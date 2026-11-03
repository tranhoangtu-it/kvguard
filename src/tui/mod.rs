//! Terminal UI (ratatui + crossterm): live view of a local or attached proxy.

pub mod app;
pub mod format;
pub mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::monitor::{Monitor, StateSnapshot, export_turns};
use crate::proxy::{CONTROL_PREFIX, DEFAULT_PORT};
use app::App;

/// Redraw / poll interval.
const TICK: Duration = Duration::from_millis(100);

/// Where the TUI reads state from.
pub enum DataSource {
    /// Proxy running in this process.
    Local(Arc<Monitor>),
    /// Proxy in another process (`kvguard attach`).
    Remote { client: reqwest::Client, url: String },
}

impl DataSource {
    pub fn remote(url: impl Into<String>) -> Self {
        Self::Remote {
            client: reqwest::Client::new(),
            url: url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn fetch(&self) -> Result<StateSnapshot> {
        match self {
            Self::Local(monitor) => Ok(monitor.snapshot()),
            Self::Remote { client, url } => client
                .get(format!("{url}{CONTROL_PREFIX}/state"))
                .timeout(Duration::from_secs(2))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
                .context("invalid state payload"),
        }
    }

    async fn toggle_heal(&self) -> Result<bool> {
        match self {
            Self::Local(monitor) => Ok(monitor.toggle_heal()),
            Self::Remote { client, url } => {
                let reply: serde_json::Value = client
                    .post(format!("{url}{CONTROL_PREFIX}/heal"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(reply
                    .get("heal_enabled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false))
            }
        }
    }
}

/// Run the full-screen TUI until the user quits. Restores the terminal on every exit path.
pub async fn run(source: DataSource) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &source).await;
    ratatui::restore();
    result
}

/// `kvguard attach`: connect to a wrapper's proxy.
pub async fn attach(url: Option<String>) -> Result<()> {
    let url = url
        .or_else(crate::wrapper::read_attach_url)
        .unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_PORT}"));
    let source = DataSource::remote(&url);
    source.fetch().await.with_context(|| {
        format!("no kvguard proxy at {url} — start one with `kvguard -- <command>`")
    })?;
    run(source).await
}

enum Action {
    Continue,
    Quit,
}

async fn event_loop(terminal: &mut DefaultTerminal, source: &DataSource) -> Result<()> {
    let mut app = App::default();
    loop {
        match source.fetch().await {
            Ok(snapshot) => app.set_snapshot(snapshot),
            Err(err) => app.flash(format!("proxy unreachable: {err}")),
        }
        terminal.draw(|frame| ui::render(frame, &app))?;

        // crossterm's poll blocks; keep it off the async workers' critical path.
        let event = tokio::task::block_in_place(|| -> Result<Option<Event>> {
            if event::poll(TICK)? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        })?;
        let Some(Event::Key(key)) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let is_ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if is_ctrl_c {
            return Ok(());
        }
        if let Action::Quit = handle_key(key.code, &mut app, source).await {
            return Ok(());
        }
    }
}

async fn handle_key(code: KeyCode, app: &mut App, source: &DataSource) -> Action {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('d') => app.toggle_view(),
        KeyCode::Char('f') => app.toggle_filter(),
        KeyCode::Char('h' | 'H') => match source.toggle_heal().await {
            Ok(true) => {
                app.flash("auto-heal ON (from the next request; expect one prefix rebuild)");
            }
            Ok(false) => app.flash("auto-heal OFF"),
            Err(err) => app.flash(format!("toggle failed: {err}")),
        },
        KeyCode::Char('e') => {
            let path = PathBuf::from(format!(
                "kvguard-trace-{}.jsonl",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ));
            let turns: Vec<_> = app.visible().into_iter().cloned().collect();
            match export_turns(&turns, &path) {
                Ok(n) => app.flash(format!("exported {n} turn(s) to {}", path.display())),
                Err(err) => app.flash(format!("export failed: {err:#}")),
            }
        }
        _ => {}
    }
    Action::Continue
}
