//! Shared test helpers for roz-mavlink integration tests.
//!
//! This file lives at `tests/common/mod.rs` (directory form) so cargo does
//! NOT treat each helper as a standalone integration test binary. Each
//! `tests/*.rs` file that needs these helpers declares `mod common;` at
//! its top.

#![allow(dead_code)]

pub mod mock_log_transport;
