//! kvguard — local proxy & TUI that catches prompt/KV cache prefix misses
//! and optionally heals requests before they reach the model.

pub mod canonicalizer;
pub mod ci;
pub mod cli;
pub mod config;
pub mod diff;
pub mod lineage;
pub mod monitor;
pub mod pricing;
pub mod provider;
pub mod proxy;
pub mod secure_file;
pub mod tui;
pub mod wrapper;
