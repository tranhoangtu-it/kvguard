//! Session lineage tree: matches each request to the conversation branch it continues.
//!
//! Matching is block-based (hash per tool / system part / message), which keeps it cheap
//! enough to run inline on the request path. Token-level detail is computed later, and
//! only for requests that turn out to be broken.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::block_hasher::{BlockKind, ContextBlock, decompose};
use crate::canonicalizer::slot_router::SlotPool;
use crate::provider::ApiKind;

/// Share of the parent's messages that must reappear for a broken request to be
/// attributed to that parent instead of starting a new branch.
const MUTATION_OVERLAP_THRESHOLD: f64 = 0.7;
/// Upper bound on tracked branches; the least recently used non-main branch is dropped.
const MAX_BRANCHES: usize = 64;

/// A processed request as seen by the lineage tree.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub api: ApiKind,
    pub model: String,
    pub blocks: Vec<ContextBlock>,
    pub body: Arc<Value>,
}

impl Snapshot {
    pub fn new(api: ApiKind, body: Arc<Value>) -> Self {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let blocks = decompose(api, &body);
        Self {
            api,
            model,
            blocks,
            body,
        }
    }

    fn size(&self) -> usize {
        self.blocks.iter().map(|b| b.text.len()).sum()
    }

    fn message_hashes(&self) -> impl Iterator<Item = u64> + '_ {
        self.blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Message)
            .map(|b| b.hash)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// First request of a new conversation branch (main session, sub-agent, title generator…).
    NewBranch,
    /// The parent request is an exact block prefix of this one (or vice versa): cache-friendly.
    Continuation,
    /// Same conversation, but an earlier block changed: the provider cache breaks here.
    Mutation,
}

/// Result of attaching a request to the tree.
#[derive(Debug, Clone)]
pub struct MatchOutcome {
    pub branch_id: u64,
    pub branch_label: String,
    pub slot: Option<u32>,
    pub kind: MatchKind,
    /// The previous request on the same branch (absent for new branches).
    pub parent: Option<Snapshot>,
    /// Number of leading blocks shared with the parent.
    pub common_blocks: usize,
}

#[derive(Debug)]
struct Branch {
    id: u64,
    ordinal: u64,
    last: Snapshot,
    last_seen: u64,
}

/// In-memory set of active conversation branches.
#[derive(Debug, Default)]
pub struct SessionTree {
    branches: Vec<Branch>,
    main: Option<u64>,
    next_id: u64,
    next_ordinal: u64,
    clock: u64,
    slots: Option<SlotPool>,
}

impl SessionTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable llama-server slot routing with `slots` parallel slots.
    /// Idempotent: re-detecting the same slot count keeps current assignments.
    pub fn set_slots(&mut self, slots: u32) {
        let unchanged = self
            .slots
            .as_ref()
            .is_some_and(|pool| pool.slot_count() == slots as usize);
        if !unchanged {
            self.slots = SlotPool::new(slots);
        }
    }

    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }

    /// Attach `snap` to its parent branch (or create a new branch) and return the verdict.
    pub fn observe(&mut self, snap: Snapshot) -> MatchOutcome {
        self.clock += 1;
        let (idx, kind, common_blocks) = match self.find_parent(&snap) {
            Some(found) => found,
            None => (self.create_branch(&snap), MatchKind::NewBranch, 0),
        };

        let branch = &mut self.branches[idx];
        branch.last_seen = self.clock;
        let parent = match kind {
            MatchKind::NewBranch => None,
            MatchKind::Continuation | MatchKind::Mutation => {
                Some(std::mem::replace(&mut branch.last, snap))
            }
        };
        let branch_id = branch.id;
        let ordinal = branch.ordinal;
        self.update_main(idx);

        let is_main = self.main == Some(branch_id);
        let clock = self.clock;
        let slot = self
            .slots
            .as_mut()
            .map(|pool| pool.assign(branch_id, is_main, clock));
        let branch_label = if is_main {
            "main".to_string()
        } else {
            format!("sub-{ordinal}")
        };
        MatchOutcome {
            branch_id,
            branch_label,
            slot,
            kind,
            parent,
            common_blocks,
        }
    }

    fn find_parent(&self, snap: &Snapshot) -> Option<(usize, MatchKind, usize)> {
        let candidates = self
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.last.api.same_layout(snap.api) && b.last.model == snap.model);

        let mut best_continuation: Option<(usize, usize)> = None;
        let mut best_mutation: Option<(usize, f64, usize)> = None;
        for (idx, branch) in candidates {
            let parent = &branch.last;
            let lcp = common_prefix_len(&parent.blocks, &snap.blocks);
            let is_prefix_extension = lcp == parent.blocks.len() || lcp == snap.blocks.len();
            if lcp > 0 && is_prefix_extension {
                if best_continuation.is_none_or(|(_, best)| lcp > best) {
                    best_continuation = Some((idx, lcp));
                }
                continue;
            }
            let overlap = message_overlap(parent, snap);
            let same_conversation =
                overlap >= MUTATION_OVERLAP_THRESHOLD || same_anchor(parent, snap);
            if same_conversation && best_mutation.is_none_or(|(_, o, l)| (overlap, lcp) > (o, l)) {
                best_mutation = Some((idx, overlap, lcp));
            }
        }
        best_continuation
            .map(|(idx, lcp)| (idx, MatchKind::Continuation, lcp))
            .or_else(|| best_mutation.map(|(idx, _, lcp)| (idx, MatchKind::Mutation, lcp)))
    }

    fn create_branch(&mut self, snap: &Snapshot) -> usize {
        if self.branches.len() >= MAX_BRANCHES {
            self.evict_lru();
        }
        self.next_id += 1;
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        self.branches.push(Branch {
            id: self.next_id,
            ordinal,
            last: snap.clone(),
            last_seen: self.clock,
        });
        self.branches.len() - 1
    }

    fn evict_lru(&mut self) {
        let main = self.main;
        if let Some(pos) = self
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| Some(b.id) != main)
            .min_by_key(|(_, b)| b.last_seen)
            .map(|(i, _)| i)
        {
            self.branches.remove(pos);
        }
    }

    /// The main branch is the one carrying the largest context.
    fn update_main(&mut self, idx: usize) {
        let candidate = &self.branches[idx];
        let current_main_size = self
            .main
            .and_then(|id| self.branches.iter().find(|b| b.id == id))
            .map(|b| b.last.size());
        match current_main_size {
            Some(size) if candidate.last.size() <= size => {}
            _ => self.main = Some(candidate.id),
        }
    }
}

/// Number of leading blocks with identical hashes.
pub fn common_prefix_len(a: &[ContextBlock], b: &[ContextBlock]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(x, y)| x.hash == y.hash)
        .count()
}

/// Same system prompt and same opening message: the same conversation even when a later
/// message was rewritten (short histories fall below the overlap threshold after one edit).
fn same_anchor(parent: &Snapshot, snap: &Snapshot) -> bool {
    let system = |s: &Snapshot| -> Vec<u64> {
        s.blocks
            .iter()
            .filter(|b| b.kind == BlockKind::System)
            .map(|b| b.hash)
            .collect()
    };
    let first_message = |s: &Snapshot| s.message_hashes().next();
    first_message(parent).is_some()
        && first_message(parent) == first_message(snap)
        && system(parent) == system(snap)
}

/// Fraction of the parent's message blocks that appear (anywhere) in the new request.
fn message_overlap(parent: &Snapshot, snap: &Snapshot) -> f64 {
    let current: HashSet<u64> = snap.message_hashes().collect();
    let (total, shared) = parent.message_hashes().fold((0usize, 0usize), |(t, s), h| {
        (t + 1, s + usize::from(current.contains(&h)))
    });
    if total == 0 {
        0.0
    } else {
        shared as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(body: Value) -> Snapshot {
        Snapshot::new(ApiKind::AnthropicMessages, Arc::new(body))
    }

    fn convo(system: &str, msgs: &[&str]) -> Value {
        let messages: Vec<Value> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({"role": role, "content": m})
            })
            .collect();
        json!({"model": "claude-sonnet-4-5", "system": system, "messages": messages})
    }

    #[test]
    fn appended_turn_is_continuation() {
        let mut tree = SessionTree::new();
        let first = tree.observe(snap(convo("S", &["a"])));
        assert_eq!(first.kind, MatchKind::NewBranch);
        let out = tree.observe(snap(convo("S", &["a", "b", "c"])));
        assert_eq!(out.kind, MatchKind::Continuation);
        assert_eq!(out.branch_label, "main");
    }

    #[test]
    fn changed_system_with_same_history_is_mutation() {
        let mut tree = SessionTree::new();
        tree.observe(snap(convo("S1", &["a", "b", "c"])));
        let out = tree.observe(snap(convo("S2", &["a", "b", "c", "d", "e"])));
        assert_eq!(out.kind, MatchKind::Mutation);
        assert_eq!(out.common_blocks, 0);
    }

    #[test]
    fn rewritten_message_in_short_history_is_mutation() {
        let mut tree = SessionTree::new();
        tree.observe(snap(convo("S", &["task", "long answer", "go"])));
        let out = tree.observe(snap(convo(
            "S",
            &["task", "[compacted]", "go", "ok", "next"],
        )));
        assert_eq!(out.kind, MatchKind::Mutation);
        assert_eq!(out.branch_label, "main");
    }

    #[test]
    fn ollama_native_and_openai_endpoints_share_a_branch() {
        let mut tree = SessionTree::new();
        let body = |msgs: Value| json!({"model": "qwen", "messages": msgs});
        let first = json!([{"role": "user", "content": "a"}]);
        let second = json!([{"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"}, {"role": "user", "content": "c"}]);
        tree.observe(Snapshot::new(ApiKind::OpenAiChat, Arc::new(body(first))));
        let out = tree.observe(Snapshot::new(ApiKind::OllamaChat, Arc::new(body(second))));
        assert_eq!(out.kind, MatchKind::Continuation);
    }

    #[test]
    fn unrelated_request_starts_new_branch() {
        let mut tree = SessionTree::new();
        tree.observe(snap(convo("main system", &["a", "b", "c"])));
        let out = tree.observe(snap(convo("sub-agent system", &["find files"])));
        assert_eq!(out.kind, MatchKind::NewBranch);
        assert_eq!(out.branch_label, "sub-1");
        assert_eq!(tree.branch_count(), 2);
    }
}
