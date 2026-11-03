//! kvguard — local proxy & TUI that catches prompt/KV cache prefix misses
//! and optionally heals requests before they reach the model.

pub mod canonicalizer;
pub mod config;
pub mod diff;
pub mod lineage;
pub mod provider;
