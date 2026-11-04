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
/// Default share of the expected cache read below which a turn is reported as PARTIAL.
pub const DEFAULT_PARTIAL_THRESHOLD: f64 = 0.8;
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
    /// Prefix matched, but the provider read only part of what the previous turn cached
    /// (provider-side cache collapse, lookback window exceeded).
    Partial,
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
    /// Previous turn of the same branch.
    #[serde(default)]
    pub parent_turn: Option<u64>,
    /// Tokens the provider should serve from cache: what the parent turn read or wrote
    /// (provider-reported, so it uses the provider's own tokenizer).
    #[serde(default)]
    pub expected_cached_tokens: Option<u64>,
    /// The request asked for Anthropic's 1-hour cache TTL (cache writes cost 2× input).
    #[serde(default)]
    pub cache_ttl_1h: bool,
    /// Extra findings (lookback window, rate limits) shown next to the root cause.
    #[serde(default)]
    pub notes: Vec<String>,
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
            parent_turn: None,
            expected_cached_tokens: None,
            cache_ttl_1h: false,
            notes: Vec::new(),
        }
    }

    /// `(expected, cached)` when the provider reported a cache read for a prompt large
    /// enough to be cached at all.
    pub fn cache_expectation(&self) -> Option<(u64, u64)> {
        let usage = self.usage.as_ref()?;
        let cached = usage.cached_tokens?;
        let min = crate::pricing::min_cacheable_tokens(&self.model);
        match self.expected_cached_tokens {
            Some(expected) => (expected >= min).then_some((expected, cached)),
            // No parent measurement: only an empty read of a large prompt is meaningful.
            None => (cached == 0 && usage.prompt_tokens.unwrap_or(0) >= min).then_some((1, 0)),
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
    fn derive_status(&self, partial_threshold: f64) -> TurnStatus {
        if self.error.is_some() {
            return TurnStatus::Error;
        }
        if self.diagnosis.is_some() || self.match_kind == MatchKind::Mutation {
            return TurnStatus::Bust;
        }
        if self.match_kind == MatchKind::NewBranch {
            return TurnStatus::NewBranch;
        }
        match self.cache_expectation() {
            Some((_, 0)) => TurnStatus::Miss,
            Some((expected, cached)) if (cached as f64) < partial_threshold * expected as f64 => {
                TurnStatus::Partial
            }
            _ if self.heal_actions.is_empty() => TurnStatus::Hit,
            _ => TurnStatus::Healed,
        }
    }

    /// Tokens re-billed / re-prefilled because they were not served from cache.
    fn lost_tokens(&self) -> u64 {
        let provider_shortfall = || {
            self.cache_expectation()
                .filter(|(expected, _)| self.expected_cached_tokens == Some(*expected))
                .map(|(expected, cached)| expected.saturating_sub(cached))
        };
        match self.status {
            TurnStatus::Partial | TurnStatus::Miss | TurnStatus::Bust => provider_shortfall()
                .or_else(|| self.diagnosis.as_ref().map(|d| d.invalidated_tokens as u64))
                .unwrap_or(0),
            _ => 0,
        }
    }
}

/// Running totals across every turn of the process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub turns: u64,
    pub busts: u64,
    pub healed: u64,
    #[serde(default)]
    pub partials: u64,
    #[serde(default)]
    pub misses: u64,
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
            partials: u64::from(record.status == TurnStatus::Partial),
            misses: u64::from(record.status == TurnStatus::Miss),
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
        self.partials += o.partials;
        self.misses += o.misses;
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
        self.partials = self.partials.saturating_sub(o.partials);
        self.misses = self.misses.saturating_sub(o.misses);
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
    partial_threshold: Mutex<f64>,
}

impl Monitor {
    pub fn new(heal: bool, prefill_tok_per_sec: f64) -> Self {
        Self {
            heal: AtomicBool::new(heal),
            next_id: AtomicU64::new(1),
            inner: RwLock::new(Inner::default()),
            prefill_tok_per_sec: Mutex::new(prefill_tok_per_sec),
            listen: RwLock::new(String::new()),
            partial_threshold: Mutex::new(DEFAULT_PARTIAL_THRESHOLD),
        }
    }

    /// Share of the expected cache read below which a turn is PARTIAL (clamped to 0..=1).
    pub fn set_partial_threshold(&self, threshold: f64) {
        if let Ok(mut current) = self.partial_threshold.lock() {
            *current = threshold.clamp(0.0, 1.0);
        }
    }

    fn partial_threshold(&self) -> f64 {
        self.partial_threshold
            .lock()
            .map(|v| *v)
            .unwrap_or(DEFAULT_PARTIAL_THRESHOLD)
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
        let (tok_per_sec, threshold) = (self.prefill_speed(), self.partial_threshold());
        let Ok(mut inner) = self.inner.write() else {
            return id;
        };
        record.expected_cached_tokens =
            expected_from_parent(&inner.turns, record.parent_turn, record.api);
        refresh_record(&mut record, tok_per_sec, threshold);
        inner.totals.add(&Totals::of(&record));
        inner.turns.push_back(record);
        while inner.turns.len() > MAX_TURNS {
            inner.turns.pop_front();
        }
        id
    }

    /// Mutate a stored turn; status, costs and totals are recomputed.
    pub fn update(&self, id: u64, change: impl FnOnce(&mut TurnRecord)) {
        let (tok_per_sec, threshold) = (self.prefill_speed(), self.partial_threshold());
        let usage = {
            let Ok(mut inner) = self.inner.write() else {
                return;
            };
            let Some(pos) = inner.turns.iter().position(|t| t.id == id) else {
                return;
            };
            let before = Totals::of(&inner.turns[pos]);
            // The parent's usage may have arrived after this turn was registered.
            let expected = expected_from_parent(
                &inner.turns,
                inner.turns[pos].parent_turn,
                inner.turns[pos].api,
            );
            let record = &mut inner.turns[pos];
            let usage_before = record.usage.clone();
            change(record);
            if expected.is_some() {
                record.expected_cached_tokens = expected;
            }
            refresh_record(record, tok_per_sec, threshold);
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

/// Prometheus text exposition of the running totals (`GET /__kvguard/metrics`).
pub fn render_metrics(snapshot: &StateSnapshot) -> String {
    let t = &snapshot.totals;
    let counters: [(&str, &str, f64); 11] = [
        ("kvguard_turns_total", "LLM requests seen", t.turns as f64),
        (
            "kvguard_busts_total",
            "Requests that broke their parent's prefix",
            t.busts as f64,
        ),
        (
            "kvguard_partials_total",
            "Intact prefixes only partly served from cache",
            t.partials as f64,
        ),
        (
            "kvguard_misses_total",
            "Intact prefixes served nothing from cache",
            t.misses as f64,
        ),
        (
            "kvguard_healed_total",
            "Requests rewritten by heal rules",
            t.healed as f64,
        ),
        (
            "kvguard_prompt_tokens_total",
            "Provider-reported prompt tokens",
            t.prompt_tokens as f64,
        ),
        (
            "kvguard_cached_tokens_total",
            "Provider-reported cached prompt tokens",
            t.cached_tokens as f64,
        ),
        (
            "kvguard_saved_usd_total",
            "USD saved by cache reads (list prices)",
            t.saved_usd,
        ),
        (
            "kvguard_wasted_usd_total",
            "USD lost to cache misses (list prices)",
            t.wasted_usd,
        ),
        (
            "kvguard_gpu_saved_seconds_total",
            "Local prefill seconds avoided",
            t.gpu_saved_s,
        ),
        (
            "kvguard_gpu_wasted_seconds_total",
            "Local prefill seconds lost",
            t.gpu_wasted_s,
        ),
    ];
    let mut out = String::new();
    for (name, help, value) in counters {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
        ));
    }
    if let Some(rate) = t.hit_rate() {
        out.push_str(&format!(
            "# HELP kvguard_cache_hit_ratio Cached share of prompt tokens\n\
             # TYPE kvguard_cache_hit_ratio gauge\nkvguard_cache_hit_ratio {rate}\n"
        ));
    }
    out.push_str(&format!(
        "# HELP kvguard_heal_enabled Auto-heal state\n# TYPE kvguard_heal_enabled gauge\n\
         kvguard_heal_enabled {}\n",
        u8::from(snapshot.heal_enabled)
    ));
    out
}

/// One-line status for shell prompts and editor status lines.
pub fn render_status_line(snapshot: &StateSnapshot) -> String {
    let t = &snapshot.totals;
    let hit = t
        .hit_rate()
        .map_or_else(|| "–".to_string(), |r| format!("{:.0}%", r * 100.0));
    let mode = if snapshot.heal_enabled { "⚡" } else { "🔍" };
    let mut line = format!("kvguard {mode} {hit} hit · ${:.2} saved", t.saved_usd);
    for (count, label) in [
        (t.busts, "bust"),
        (t.partials, "partial"),
        (t.misses, "miss"),
    ] {
        if count > 0 {
            line.push_str(&format!(" · {count} {label}"));
        }
    }
    line
}

/// What the provider should serve from cache on the next turn of a branch: the prefix the
/// parent turn read or wrote (Anthropic), or the parent's whole prompt (OpenAI, llama.cpp).
fn expected_from_parent(
    turns: &VecDeque<TurnRecord>,
    parent: Option<u64>,
    api: ApiKind,
) -> Option<u64> {
    let parent = parent?;
    let usage = turns
        .iter()
        .rev()
        .find(|t| t.id == parent)?
        .usage
        .as_ref()?;
    match api {
        ApiKind::AnthropicMessages => {
            let cached = usage.cached_tokens.unwrap_or(0) + usage.cache_write_tokens.unwrap_or(0);
            (cached > 0).then_some(cached)
        }
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses | ApiKind::OllamaChat => usage.prompt_tokens,
    }
}

fn refresh_record(record: &mut TurnRecord, tok_per_sec: f64, partial_threshold: f64) {
    record.status = record.derive_status(partial_threshold);
    let cached = record
        .usage
        .as_ref()
        .and_then(|u| u.cached_tokens)
        .unwrap_or(0);
    let lost = record.lost_tokens();
    if record.local {
        record.saved_usd = 0.0;
        record.wasted_usd = 0.0;
        record.gpu_saved_s = pricing::prefill_seconds(cached, tok_per_sec);
        record.gpu_wasted_s = pricing::prefill_seconds(lost, tok_per_sec);
    } else if let Some(price) = pricing::price_for(&record.model) {
        let one_hour = record.cache_ttl_1h && record.api == ApiKind::AnthropicMessages;
        record.saved_usd = pricing::saved_usd(price, cached);
        record.wasted_usd = pricing::wasted_usd(price, lost, one_hour);
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

    fn usage(prompt: u64, cached: u64, written: u64) -> Usage {
        Usage {
            prompt_tokens: Some(prompt),
            cached_tokens: Some(cached),
            cache_write_tokens: Some(written),
            ..Usage::default()
        }
    }

    #[test]
    fn collapsed_cache_read_on_intact_prefix_is_partial() {
        let monitor = Monitor::new(false, 850.0);
        let parent = monitor.add_turn(record(MatchKind::NewBranch));
        monitor.update(parent, |t| t.usage = Some(usage(640_300, 600_000, 40_212)));

        let mut child = record(MatchKind::Continuation);
        child.parent_turn = Some(parent);
        let id = monitor.add_turn(child);
        monitor.update(id, |t| t.usage = Some(usage(641_000, 25_165, 615_835)));

        let snap = monitor.snapshot();
        let turn = snap.turns.iter().find(|t| t.id == id).unwrap();
        assert_eq!(turn.status, TurnStatus::Partial);
        assert_eq!(turn.expected_cached_tokens, Some(640_212));
        // (640,212 − 25,165) tokens re-written at Sonnet's 5-minute rates: $3.75 − $0.30 per M.
        let expected_waste = (640_212.0 - 25_165.0) / 1e6 * (3.75 - 0.3);
        assert!((turn.wasted_usd - expected_waste).abs() < 1e-9);
        assert_eq!(snap.totals.partials, 1);
    }

    #[test]
    fn one_hour_ttl_doubles_write_cost() {
        let monitor = Monitor::new(false, 850.0);
        let parent = monitor.add_turn(record(MatchKind::NewBranch));
        monitor.update(parent, |t| t.usage = Some(usage(10_000, 0, 10_000)));
        let mut child = record(MatchKind::Continuation);
        child.parent_turn = Some(parent);
        child.cache_ttl_1h = true;
        let id = monitor.add_turn(child);
        monitor.update(id, |t| t.usage = Some(usage(10_500, 0, 10_500)));
        let turn = monitor
            .snapshot()
            .turns
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(turn.status, TurnStatus::Miss);
        assert!((turn.wasted_usd - 10_000.0 / 1e6 * (6.0 - 0.3)).abs() < 1e-9);
    }

    #[test]
    fn small_prompts_never_raise_cache_alerts() {
        let monitor = Monitor::new(false, 850.0);
        let parent = monitor.add_turn(record(MatchKind::NewBranch));
        monitor.update(parent, |t| t.usage = Some(usage(600, 0, 600)));
        let mut child = record(MatchKind::Continuation);
        child.parent_turn = Some(parent);
        let id = monitor.add_turn(child);
        monitor.update(id, |t| t.usage = Some(usage(700, 0, 700)));
        let turn = monitor
            .snapshot()
            .turns
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(turn.status, TurnStatus::Hit);
    }

    #[test]
    fn metrics_and_status_line_render_totals() {
        let monitor = Monitor::new(true, 850.0);
        let id = monitor.add_turn(record(MatchKind::Continuation));
        monitor.update(id, |t| t.usage = Some(usage(10_000, 9_000, 0)));
        let snap = monitor.snapshot();
        let metrics = render_metrics(&snap);
        assert!(metrics.contains("kvguard_turns_total 1"));
        assert!(metrics.contains("kvguard_cached_tokens_total 9000"));
        assert!(metrics.contains("kvguard_cache_hit_ratio 0.9"));
        assert!(metrics.contains("# TYPE kvguard_partials_total counter"));
        let line = render_status_line(&snap);
        assert!(line.starts_with("kvguard ⚡ 90% hit"), "{line}");
    }

    #[test]
    fn toggle_returns_new_state() {
        let monitor = Monitor::new(false, 850.0);
        assert!(monitor.toggle_heal());
        assert!(!monitor.toggle_heal());
    }
}
