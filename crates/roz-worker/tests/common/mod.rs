//! Shared helpers for roz-worker integration tests.
//!
//! `fleet` — spawn/shutdown real `roz-worker` binaries via `tokio::process`.
//! Used by `multi_worker_fleet.rs`, `dual_publish_fullstack_chaos.rs`,
//! and `worker_startup_sanity.rs` integration tests (Phase 16 ZEN-TEST-*).
//!
//! `mock_log_transport` — Phase 26.8-08 cross-crate copy of
//! `roz-mavlink/tests/common/mock_log_transport.rs` (see that file for
//! duplication rationale). Consumed by `ulog_session_finalize_integration`.

pub mod fleet;
pub mod mock_log_transport;
