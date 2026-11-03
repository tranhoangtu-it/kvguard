//! Shared live state: per-turn records, running totals and the heal toggle.
//!
//! The proxy writes records, the TUI / `attach` endpoint / exit summary read snapshots.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::diff::Diagnosis;
use crate::lineage::MatchKind;
use crate::pricing;
use crate::provider::ApiKind;

/// Turns kept in memory for display; totals still cover every turn.
const MAX_TURNS: usize = 2_000;
/// Below this many prompt tokens providers do not cache at all, so a zero read is not a miss.
const MIN_CACHEABLE_TOKENS: u64 = 1_024;
/// Weight of the newest prefill-speed measurement in the moving average.
const PREFILL_EMA_WEIGHT: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// Waiting for the upstream response / analysis.
    Pending,
    /// First request of a branch.
    NewBranch,
    /// Prefix matched the parent turn.
    Hit,
    /// Prefix matched after heal rules rewrote the request.
    Healed,
    /// Prefix matched, but the provider still reported no cache read (TTL expiry, eviction).
    Miss,
    /// Prefix broke against the parent turn.
    Bust,
    /// Upstream unreachable or returned an error.
    Error,
}

/// Prompt-cache usage reported by the upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// All prompt tokens (cached + uncached).
    pub prompt_tokens: Option<u64>,
    /// Tokens served from the provider / KV cache.
    pub cached_tokens: Option<u64>,
    /// Tokens written to the cache (Anthropic `cache_creation_input_tokens`).
    pub cache_write_tokens: Option<u64>,
    /// Tokens that had to be prefilled (llama.cpp `prompt_n`, Ollama `prompt_eval_count`).
    pub prefilled_tokens: Option<u64>,
    /// Prefill wall time in milliseconds.
    pub prompt_ms: Option<f64>,
}

/// One intercepted LLM request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub id: u64,
    pub timestamp: String,
    pub api: ApiKind,
    pub model: String,
    pub branch: String,
    pub slot: Option<u32>,
    /// Upstream is a local inference server (cost is GPU time, not USD).
    pub local: bool,
    pub match_kind: MatchKind,
    pub status: TurnStatus,
    pub heal_actions: Vec<String>,
    pub diagnosis: Option<Diagnosis>,
    /// Estimated prompt tokens (o200k).
    pub est_prompt_tokens: Option<u64>,
    /// Estimated tokens reusable from the parent turn's prefix.
    pub est_reused_tokens: Option<u64>,
    pub usage: Option<Usage>,
    pub http_status: Option<u16>,
    pub error: Option<String>,
    pub saved_usd: f64,
    pub wasted_usd: f64,
    pub gpu_saved_s: f64,
    pub gpu_wasted_s: f64,
}

impl TurnRecord {
    pub fn new(api: ApiKind, model: String, branch: String, match_kind: MatchKind) -> Self {
        Self {
            id: 0,
            timestamp: chrono::Local::now().to_rfc3339(),
            api,
            model,
            branch,
            slot: None,
            local: false,
            match_kind,
            status: TurnStatus::Pending,
            heal_actions: Vec::new(),
            diagnosis: None,
            est_prompt_tokens: None,
            est_reused_tokens: None,
            usage: None,
            http_status: None,
            error: None,
            saved_usd: 0.0,
            wasted_usd: 0.0,
            gpu_saved_s: 0.0,
            gpu_wasted_s: 0.0,
        }
    }

    /// Cached share of the prompt: provider-reported when available, otherwise estimated.
    pub fn hit_ratio(&self) -> Option<f64> {
        let reported = self
            .usage
            .as_ref()
            .and_then(|u| Some((u.cached_tokens?, u.prompt_tokens?)));
        let (cached, total) =
            reported.or_else(|| Some((self.est_reused_tokens?, self.est_prompt_tokens?)))?;
        (total > 0).then(|| (cached as f64 / total as f64).min(1.0))
    }

    /// Status derived from the lineage verdict, analysis and provider usage.
    fn derive_status(&self) -> TurnStatus {
        if self.error.is_some() {
            return TurnStatus::Error;
        }
        if self.diagnosis.is_some() || self.match_kind == MatchKind::Mutation {
            return TurnStatus::Bust;
        }
        if self.match_kind == MatchKind::NewBranch {
            return TurnStatus::NewBranch;
        }
        let provider_missed = self.usage.as_ref().is_some_and(|u| {
            u.cached_tokens == Some(0) && u.prompt_tokens.unwrap_or(0) >= MIN_CACHEABLE_TOKENS
        });
        if provider_missed {
            TurnStatus::Miss
        } else if self.heal_actions.is_empty() {
            TurnStatus::Hit
        } else {
            TurnStatus::Healed
        }
    }
}

/// Running totals across every turn of the process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub turns: u64,
    pub busts: u64,
    pub healed: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub est_prompt_tokens: u64,
    pub est_reused_tokens: u64,
    pub saved_usd: f64,
    pub wasted_usd: f64,
    pub gpu_saved_s: f64,
    pub gpu_wasted_s: f64,
}

impl Totals {
    fn of(record: &TurnRecord) -> Self {
        let usage = record.usage.clone().unwrap_or_default();
        Self {
            turns: 1,
            busts: u64::from(record.status == TurnStatus::Bust),
            healed: u64::from(record.status == TurnStatus::Healed),
            prompt_tokens: usage.prompt_tokens.unwrap_or(0),
            cached_tokens: usage.cached_tokens.unwrap_or(0),
            est_prompt_tokens: record.est_prompt_tokens.unwrap_or(0),
            est_reused_tokens: record.est_reused_tokens.unwrap_or(0),
            saved_usd: record.saved_usd,
            wasted_usd: record.wasted_usd,
            gpu_saved_s: record.gpu_saved_s,
            gpu_wasted_s: record.gpu_wasted_s,
        }
    }

    fn add(&mut self, o: &Self) {
        self.turns += o.turns;
        self.busts += o.busts;
        self.healed += o.healed;
        self.prompt_tokens += o.prompt_tokens;
        self.cached_tokens += o.cached_tokens;
        self.est_prompt_tokens += o.est_prompt_tokens;
        self.est_reused_tokens += o.est_reused_tokens;
        self.saved_usd += o.saved_usd;
        self.wasted_usd += o.wasted_usd;
        self.gpu_saved_s += o.gpu_saved_s;
        self.gpu_wasted_s += o.gpu_wasted_s;
    }

    fn sub(&mut self, o: &Self) {
        self.turns = self.turns.saturating_sub(o.turns);
        self.busts = self.busts.saturating_sub(o.busts);
        self.healed = self.healed.saturating_sub(o.healed);
        self.prompt_tokens = self.prompt_tokens.saturating_sub(o.prompt_tokens);
        self.cached_tokens = self.cached_tokens.saturating_sub(o.cached_tokens);
        self.est_prompt_tokens = self.est_prompt_tokens.saturating_sub(o.est_prompt_tokens);
        self.est_reused_tokens = self.est_reused_tokens.saturating_sub(o.est_reused_tokens);
        self.saved_usd -= o.saved_usd;
        self.wasted_usd -= o.wasted_usd;
        self.gpu_saved_s -= o.gpu_saved_s;
        self.gpu_wasted_s -= o.gpu_wasted_s;
    }

    /// Provider-reported hit rate, falling back to the estimate when no usage was reported.
    pub fn hit_rate(&self) -> Option<f64> {
        if self.prompt_tokens > 0 {
            Some(self.cached_tokens as f64 / self.prompt_tokens as f64)
        } else if self.est_prompt_tokens > 0 {
            Some(self.est_reused_tokens as f64 / self.est_prompt_tokens as f64)
        } else {
            None
        }
    }

    /// Tokens that did not have to be billed / prefilled again.
    pub fn tokens_saved(&self) -> u64 {
        if self.prompt_tokens > 0 {
            self.cached_tokens
        } else {
            self.est_reused_tokens
        }
    }
}

/// Serializable view used by the TUI and `kvguard attach`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub heal_enabled: bool,
    pub listen: String,
    pub totals: Totals,
    pub turns: Vec<TurnRecord>,
}

#[derive(Debug, Default)]
struct Inner {
    turns: VecDeque<TurnRecord>,
    totals: Totals,
}

/// Process-wide monitor.
#[derive(Debug)]
pub struct Monitor {
    heal: AtomicBool,
    next_id: AtomicU64,
    inner: RwLock<Inner>,
    prefill_tok_per_sec: Mutex<f64>,
    listen: RwLock<String>,
}

impl Monitor {
    pub fn new(heal: bool, prefill_tok_per_sec: f64) -> Self {
        Self {
            heal: AtomicBool::new(heal),
            next_id: AtomicU64::new(1),
            inner: RwLock::new(Inner::default()),
            prefill_tok_per_sec: Mutex::new(prefill_tok_per_sec),
            listen: RwLock::new(String::new()),
        }
    }

    pub fn heal_enabled(&self) -> bool {
        self.heal.load(Ordering::Relaxed)
    }

    pub fn set_heal(&self, enabled: bool) {
        self.heal.store(enabled, Ordering::Relaxed);
    }

    /// Flip auto-heal and return the new state.
    pub fn toggle_heal(&self) -> bool {
        !self.heal.fetch_xor(true, Ordering::Relaxed)
    }

    pub fn set_listen(&self, addr: impl Into<String>) {
        if let Ok(mut listen) = self.listen.write() {
            *listen = addr.into();
        }
    }

    /// Store a new turn and return its id.
    pub fn add_turn(&self, mut record: TurnRecord) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        record.id = id;
        refresh_record(&mut record, self.prefill_speed());
        let Ok(mut inner) = self.inner.write() else {
            return id;
        };
        inner.totals.add(&Totals::of(&record));
        inner.turns.push_back(record);
        while inner.turns.len() > MAX_TURNS {
            inner.turns.pop_front();
        }
        id
    }

    /// Mutate a stored turn; status, costs and totals are recomputed.
    pub fn update(&self, id: u64, change: impl FnOnce(&mut TurnRecord)) {
        let tok_per_sec = self.prefill_speed();
        let usage = {
            let Ok(mut inner) = self.inner.write() else {
                return;
            };
            let Some(pos) = inner.turns.iter().position(|t| t.id == id) else {
                return;
            };
            let before = Totals::of(&inner.turns[pos]);
            let record = &mut inner.turns[pos];
            let usage_before = record.usage.clone();
            change(record);
            refresh_record(record, tok_per_sec);
            let after = Totals::of(record);
            // Calibrate only from a newly reported usage, never twice for the same turn.
            let new_usage = record
                .usage
                .clone()
                .filter(|u| Some(u) != usage_before.as_ref());
            inner.totals.sub(&before);
            inner.totals.add(&after);
            new_usage
        };
        if let Some(usage) = usage {
            self.observe_prefill(&usage);
        }
    }

    fn prefill_speed(&self) -> f64 {
        self.prefill_tok_per_sec
            .lock()
            .map(|v| *v)
            .unwrap_or(crate::config::DEFAULT_PREFILL_TOK_PER_SEC)
    }

    /// Calibrate the prefill speed from llama.cpp / Ollama timings.
    fn observe_prefill(&self, usage: &Usage) {
        let (Some(tokens), Some(ms)) = (usage.prefilled_tokens, usage.prompt_ms) else {
            return;
        };
        if tokens < 64 || ms <= 0.0 {
            return;
        }
        let measured = tokens as f64 / (ms / 1000.0);
        if let Ok(mut speed) = self.prefill_tok_per_sec.lock() {
            *speed = *speed * (1.0 - PREFILL_EMA_WEIGHT) + measured * PREFILL_EMA_WEIGHT;
        }
    }

    pub fn snapshot(&self) -> StateSnapshot {
        let (totals, turns) = self
            .inner
            .read()
            .map(|i| (i.totals.clone(), i.turns.iter().cloned().collect()))
            .unwrap_or_default();
        StateSnapshot {
            heal_enabled: self.heal_enabled(),
            listen: self.listen.read().map(|l| l.clone()).unwrap_or_default(),
            totals,
            turns,
        }
    }
}

/// Write turns as JSONL (shared by the local and attached TUI).
pub fn export_turns(turns: &[TurnRecord], path: &Path) -> Result<usize> {
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    for turn in turns {
        serde_json::to_writer(&mut file, turn)?;
        file.write_all(b"\n")?;
    }
    Ok(turns.len())
}

fn refresh_record(record: &mut TurnRecord, tok_per_sec: f64) {
    record.status = record.derive_status();
    let cached = record
        .usage
        .as_ref()
        .and_then(|u| u.cached_tokens)
        .unwrap_or(0);
    let invalidated = record
        .diagnosis
        .as_ref()
        .map(|d| d.invalidated_tokens as u64)
        .unwrap_or(0);
    if record.local {
        record.saved_usd = 0.0;
        record.wasted_usd = 0.0;
        record.gpu_saved_s = pricing::prefill_seconds(cached, tok_per_sec);
        record.gpu_wasted_s = pricing::prefill_seconds(invalidated, tok_per_sec);
    } else if let Some(price) = pricing::price_for(&record.model) {
        record.saved_usd = pricing::saved_usd(price, cached);
        record.wasted_usd = pricing::wasted_usd(price, invalidated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: MatchKind) -> TurnRecord {
        TurnRecord::new(
            ApiKind::AnthropicMessages,
            "claude-sonnet-4-5".into(),
            "main".into(),
            kind,
        )
    }

    #[test]
    fn totals_follow_updates() {
        let monitor = Monitor::new(false, 850.0);
        let id = monitor.add_turn(record(MatchKind::Continuation));
        monitor.update(id, |t| {
            t.usage = Some(Usage {
                prompt_tokens: Some(10_000),
                cached_tokens: Some(9_000),
                ..Usage::default()
            });
        });
        let snap = monitor.snapshot();
        assert_eq!(snap.totals.turns, 1);
        assert_eq!(snap.totals.cached_tokens, 9_000);
        assert!((snap.totals.hit_rate().unwrap() - 0.9).abs() < 1e-9);
        assert!(snap.totals.saved_usd > 0.0);
        assert_eq!(snap.turns[0].status, TurnStatus::Hit);
    }

    #[test]
    fn zero_cache_read_on_matched_prefix_is_a_miss() {
        let monitor = Monitor::new(false, 850.0);
        let id = monitor.add_turn(record(MatchKind::Continuation));
        monitor.update(id, |t| {
            t.usage = Some(Usage {
                prompt_tokens: Some(5_000),
                cached_tokens: Some(0),
                ..Usage::default()
            });
        });
        assert_eq!(monitor.snapshot().turns[0].status, TurnStatus::Miss);
    }

    #[test]
    fn toggle_returns_new_state() {
        let monitor = Monitor::new(false, 850.0);
        assert!(monitor.toggle_heal());
        assert!(!monitor.toggle_heal());
    }
}
