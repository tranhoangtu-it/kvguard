//! TUI state: latest snapshot, selection, filters and transient messages.

use std::time::{Duration, Instant};

use crate::monitor::{StateSnapshot, TurnRecord, TurnStatus};

const FLASH_TTL: Duration = Duration::from_secs(4);

/// What the X-Ray pane shows for the selected turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView {
    JsonAst,
    Tokens,
}

#[derive(Debug)]
pub struct App {
    pub snapshot: StateSnapshot,
    /// Index into [`App::visible`].
    pub selected: Option<usize>,
    /// Keep the newest turn selected until the user navigates.
    pub follow: bool,
    pub busts_only: bool,
    pub view: DiffView,
    flash: Option<(String, Instant)>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            snapshot: StateSnapshot::default(),
            selected: None,
            follow: true,
            busts_only: false,
            view: DiffView::JsonAst,
            flash: None,
        }
    }
}

impl App {
    pub fn visible(&self) -> Vec<&TurnRecord> {
        self.snapshot
            .turns
            .iter()
            .filter(|t| !self.busts_only || t.status == TurnStatus::Bust)
            .collect()
    }

    pub fn set_snapshot(&mut self, snapshot: StateSnapshot) {
        self.snapshot = snapshot;
        self.clamp_selection();
    }

    pub fn selected_turn(&self) -> Option<&TurnRecord> {
        let visible = self.visible();
        self.selected.and_then(|i| visible.get(i).copied())
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        let current = self.selected.unwrap_or(len - 1);
        let next = current.saturating_add_signed(delta).min(len - 1);
        self.selected = Some(next);
        self.follow = next == len - 1;
    }

    pub fn toggle_filter(&mut self) {
        self.busts_only = !self.busts_only;
        self.follow = true;
        self.clamp_selection();
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            DiffView::JsonAst => DiffView::Tokens,
            DiffView::Tokens => DiffView::JsonAst,
        };
    }

    pub fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }

    pub fn flash_message(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < FLASH_TTL)
            .map(|(m, _)| m.as_str())
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        self.selected = match (len, self.follow) {
            (0, _) => None,
            (_, true) => Some(len - 1),
            (_, false) => Some(self.selected.unwrap_or(0).min(len - 1)),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::MatchKind;
    use crate::provider::ApiKind;

    fn turn(id: u64, status: TurnStatus) -> TurnRecord {
        let mut t = TurnRecord::new(
            ApiKind::OpenAiChat,
            "m".into(),
            "main".into(),
            MatchKind::Continuation,
        );
        t.id = id;
        t.status = status;
        t
    }

    #[test]
    fn follows_latest_and_filters_busts() {
        let mut app = App::default();
        app.set_snapshot(StateSnapshot {
            turns: vec![
                turn(1, TurnStatus::Hit),
                turn(2, TurnStatus::Bust),
                turn(3, TurnStatus::Hit),
            ],
            ..StateSnapshot::default()
        });
        assert_eq!(app.selected_turn().map(|t| t.id), Some(3));
        app.move_selection(-1);
        assert_eq!(app.selected_turn().map(|t| t.id), Some(2));
        assert!(!app.follow);
        app.toggle_filter();
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.selected_turn().map(|t| t.id), Some(2));
    }
}
