//! Shared helpers for roz-worker integration tests.
//!
//! `fleet` — spawn/shutdown real `roz-worker` binaries via tokio::process.
//! Used by `multi_worker_fleet.rs`, `dual_publish_fullstack_chaos.rs`,
//! and `worker_startup_sanity.rs` integration tests (Phase 16 ZEN-TEST-*).

pub mod fleet;
