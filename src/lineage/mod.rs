//! Conversation lineage: block decomposition and the in-memory LCP session tree.

pub mod block_hasher;
pub mod lcp_tree;

pub use block_hasher::{BlockKind, ContextBlock, decompose, prompt_text};
pub use lcp_tree::{MatchKind, MatchOutcome, SessionTree, Snapshot};
