//! Rendering: KPI ribbon, turn table, X-Ray diff inspector and key bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, DiffView};
use super::format::{fmt_count, fmt_duration, fmt_tokens};
use crate::diff::Diagnosis;
use crate::monitor::{TurnRecord, TurnStatus};

const HEALTH_CELLS: usize = 10;

pub fn render(frame: &mut Frame, app: &App) {
    let [kpi, table, xray, keys] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(45),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_kpis(frame, kpi, app);
    render_turns(frame, table, app);
    render_xray(frame, xray, app);
    render_keys(frame, keys, app);
}

fn render_kpis(frame: &mut Frame, area: Rect, app: &App) {
    let totals = &app.snapshot.totals;
    let mode = if app.snapshot.heal_enabled {
        Span::styled(
            " 🔥 AUTO-HEAL: ON ",
            Style::new().fg(Color::Black).bg(Color::Green).bold(),
        )
    } else {
        Span::styled(
            " 🔍 INSPECT ONLY ",
            Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
        )
    };
    let hit = totals
        .hit_rate()
        .map_or_else(|| "–".to_string(), |r| format!("{:.1}%", r * 100.0));
    let sep = || Span::styled("  │  ", Style::new().fg(Color::DarkGray));
    let line = Line::from(vec![
        mode,
        sep(),
        Span::raw("CACHE HIT "),
        Span::styled(hit, Style::new().bold()),
        sep(),
        Span::raw("TOKENS SAVED "),
        Span::styled(
            format!("{} tok", fmt_tokens(totals.tokens_saved())),
            Style::new().bold(),
        ),
        sep(),
        Span::styled(
            format!("${:.2} saved", totals.saved_usd),
            Style::new().fg(Color::Green),
        ),
        Span::raw(" | "),
        Span::styled(
            format!("${:.2} wasted", totals.wasted_usd),
            Style::new().fg(Color::Red),
        ),
        sep(),
        Span::raw("GPU "),
        Span::styled(
            format!("+{} prefill avoided", fmt_duration(totals.gpu_saved_s)),
            Style::new().fg(Color::Cyan),
        ),
    ]);
    let title = format!(
        " kvguard v{} · {} ",
        env!("CARGO_PKG_VERSION"),
        app.snapshot.listen
    );
    frame.render_widget(
        Paragraph::new(line).block(Block::new().borders(Borders::ALL).title(title)),
        area,
    );
}

fn render_turns(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new([
        "Turn",
        "Branch / Slot",
        "Model",
        "Prefix Health",
        "Hit %",
        "Root Cause / Action",
    ])
    .style(Style::new().bold().fg(Color::Gray));
    let rows = app.visible().into_iter().map(turn_row);
    let widths = [
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Length(24),
        Constraint::Length(13),
        Constraint::Length(6),
        Constraint::Min(20),
    ];
    let filter = if app.busts_only { " [busts only]" } else { "" };
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" Requests by branch{filter} ")),
        );
    let mut state = TableState::default().with_selected(app.selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn turn_row(turn: &TurnRecord) -> Row<'static> {
    let branch = match turn.slot {
        Some(slot) => format!("{} [slot {slot}]", turn.branch),
        None => turn.branch.clone(),
    };
    let ratio = turn.hit_ratio();
    let hit = ratio.map_or_else(|| "–".to_string(), |r| format!("{:.0}%", r * 100.0));
    let (cause, color) = cause(turn);
    Row::new(vec![
        Cell::from(format!("#{:02}", turn.id)),
        Cell::from(branch),
        Cell::from(turn.model.clone()),
        Cell::from(health_bar(ratio)),
        Cell::from(hit),
        Cell::from(Span::styled(cause, Style::new().fg(color))),
    ])
}

fn health_bar(ratio: Option<f64>) -> Line<'static> {
    let Some(ratio) = ratio else {
        return Line::styled(
            format!("[{}]", "·".repeat(HEALTH_CELLS)),
            Style::new().fg(Color::DarkGray),
        );
    };
    let filled = ((ratio * HEALTH_CELLS as f64).round() as usize).min(HEALTH_CELLS);
    Line::from(vec![
        Span::raw("["),
        Span::styled("█".repeat(filled), Style::new().fg(Color::Green)),
        Span::styled(
            "░".repeat(HEALTH_CELLS - filled),
            Style::new().fg(Color::Red),
        ),
        Span::raw("]"),
    ])
}

/// Root-cause / action text and its color for a turn (also used by `--no-tui` output).
pub fn cause(turn: &TurnRecord) -> (String, Color) {
    match turn.status {
        TurnStatus::Bust => match &turn.diagnosis {
            Some(d) => (
                format!(
                    "❌ BUST @ tok ~#{} ({} · {})",
                    d.divergence_token,
                    d.kind.label(),
                    d.json_path
                ),
                Color::Red,
            ),
            None => ("❌ BUST (analyzing…)".to_string(), Color::Red),
        },
        TurnStatus::Healed => (
            format!("⚡ HEALED ({})", turn.heal_actions.join(" + ")),
            Color::Green,
        ),
        TurnStatus::Hit => ("✅ HIT".to_string(), Color::Green),
        TurnStatus::Miss => (
            "⚠ MISS: prefix intact but provider cached 0 (TTL/eviction?)".to_string(),
            Color::Yellow,
        ),
        TurnStatus::NewBranch => ("◆ NEW BRANCH".to_string(), Color::Blue),
        TurnStatus::Pending => ("…".to_string(), Color::DarkGray),
        TurnStatus::Error => (
            format!("✖ {}", turn.error.as_deref().unwrap_or("error")),
            Color::Magenta,
        ),
    }
}

fn render_xray(frame: &mut Frame, area: Rect, app: &App) {
    let view = match app.view {
        DiffView::JsonAst => "JSON AST",
        DiffView::Tokens => "Tokens",
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" X-Ray Diff Inspector [{view}] "));
    let lines = match app.selected_turn() {
        None => waiting_lines(&app.snapshot.listen),
        Some(turn) => match &turn.diagnosis {
            Some(d) => diagnosis_lines(turn, d, app.view),
            None => detail_lines(turn),
        },
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn waiting_lines(listen: &str) -> Vec<Line<'static>> {
    let hint = |text: String| Line::styled(text, Style::new().fg(Color::Cyan));
    vec![
        Line::from("Waiting for requests… point your client at the proxy:"),
        Line::from(""),
        hint(format!("  ANTHROPIC_BASE_URL={listen}")),
        hint(format!("  OPENAI_BASE_URL={listen}/v1")),
        hint(format!("  OLLAMA_HOST={listen}")),
    ]
}

fn diagnosis_lines(turn: &TurnRecord, d: &Diagnosis, view: DiffView) -> Vec<Line<'static>> {
    let impact = if turn.local {
        format!("+{} prefill", fmt_duration(turn.gpu_wasted_s))
    } else {
        format!("-${:.3}", turn.wasted_usd)
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "Divergence at Token ~#{} / {} (o200k est.)",
                    fmt_count(d.divergence_token),
                    fmt_count(d.parent_tokens)
                ),
                Style::new().bold(),
            ),
            Span::raw(format!(" │ JSON Path: {}", d.json_path)),
            Span::styled(
                format!(" │ Impact: {impact}"),
                Style::new().fg(Color::Red),
            ),
        ]),
        Line::styled(
            format!(
                "Cause: {} · ~{} tokens invalidated",
                d.kind.label(),
                fmt_count(d.invalidated_tokens)
            ),
            Style::new().fg(Color::Yellow),
        ),
        Line::from(""),
    ];
    let red = Style::new().fg(Color::Red);
    let green = Style::new().fg(Color::Green);
    match view {
        DiffView::JsonAst => {
            lines.push(Line::styled(format!("- Turn N-1: {}", d.old_value), red));
            lines.push(Line::styled(format!("+ Turn N:   {}", d.new_value), green));
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!("- {}", visible_ws(&d.parent_excerpt)),
                red,
            ));
            lines.push(Line::styled(
                format!("+ {}", visible_ws(&d.current_excerpt)),
                green,
            ));
        }
        DiffView::Tokens => {
            lines.push(token_line("- ", &d.parent_window, red));
            lines.push(token_line("+ ", &d.current_window, green));
        }
    }
    lines
}

fn token_line(prefix: &'static str, tokens: &[String], style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, style)];
    spans.extend(tokens.iter().map(|t| {
        let piece_style = if t.starts_with('»') {
            style.bold().underlined()
        } else {
            style
        };
        Span::styled(format!("[{}]", visible_ws(t)), piece_style)
    }));
    Line::from(spans)
}

fn detail_lines(turn: &TurnRecord) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "#{:02} {} · {} · {} · {:?}",
        turn.id,
        turn.branch,
        turn.api.label(),
        turn.model,
        turn.status
    ))];
    if let Some(u) = &turn.usage {
        lines.push(Line::from(format!(
            "Provider usage: prompt {} · cached {} · cache write {} · prefilled {}{}",
            opt_tokens(u.prompt_tokens),
            opt_tokens(u.cached_tokens),
            opt_tokens(u.cache_write_tokens),
            opt_tokens(u.prefilled_tokens),
            u.prompt_ms
                .map(|ms| format!(" in {}", fmt_duration(ms / 1000.0)))
                .unwrap_or_default(),
        )));
    }
    if let (Some(prompt), Some(reused)) = (turn.est_prompt_tokens, turn.est_reused_tokens) {
        lines.push(Line::from(format!(
            "Estimated (o200k): prompt {} · reusable prefix {}",
            fmt_tokens(prompt),
            fmt_tokens(reused)
        )));
    }
    if !turn.heal_actions.is_empty() {
        lines.push(Line::styled(
            format!("Heal: {}", turn.heal_actions.join(" · ")),
            Style::new().fg(Color::Green),
        ));
    }
    if let Some(error) = &turn.error {
        lines.push(Line::styled(
            format!("Error: {error}"),
            Style::new().fg(Color::Magenta),
        ));
    }
    lines
}

fn opt_tokens(value: Option<u64>) -> String {
    value.map_or_else(|| "–".to_string(), fmt_tokens)
}

fn visible_ws(text: &str) -> String {
    text.replace('\n', "⏎").replace('\t', "⇥")
}

fn render_keys(frame: &mut Frame, area: Rect, app: &App) {
    let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Black).bg(Color::Gray));
    let mut spans = vec![
        key("[h]"),
        Span::raw(" Toggle Auto-Heal  "),
        key("[d]"),
        Span::raw(" View: JSON AST / Tokens  "),
        key("[f]"),
        Span::raw(" Filter: Busts Only  "),
        key("[e]"),
        Span::raw(" Export Trace (.jsonl)  "),
        key("[q]"),
        Span::raw(" Quit"),
    ];
    if let Some(message) = app.flash_message() {
        spans.push(Span::styled(
            format!("   {message}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
