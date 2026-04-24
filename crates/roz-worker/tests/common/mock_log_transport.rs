//! File-backed mock MAVLink LOG_* transport replay helper — Phase 26.8-08 copy.
//!
//! # Duplication rationale
//!
//! This helper is a byte-identical copy of
//! `crates/roz-mavlink/tests/common/mock_log_transport.rs` (Plan 03) with
//! ONE path adjustment: `FIXTURE_PATH` points into the sibling
//! `roz-mavlink` crate via `CARGO_MANIFEST_DIR`-relative resolution.
//!
//! Cross-crate reuse of `tests/common/*.rs` is not supported by cargo:
//! `tests/*.rs` files are integration-test binaries, not library sources,
//! and cannot be depended on from sibling crates. Two alternatives were
//! considered:
//!
//! 1. **Promote the mock to `src/testing/` under a `test-helpers` feature**
//!    — invasive, pollutes the crate's public surface for a test-only type.
//!    Rejected.
//! 2. **Copy the ~200 LOC verbatim with a fixture-path adjustment** — the
//!    pragmatic choice. The protocol semantics are stable (Plan 03 locked),
//!    so drift risk is low. Divergence would be caught by the pinned
//!    `FIXTURE_DIGEST_SHA256_HEX` test in both crates.
//!
//! If a future phase lifts the mock into a published library crate (e.g.
//! `roz-mavlink-test`), this file and its sibling in `roz-mavlink/` both
//! delete in favor of a single `pub use`.

#![allow(dead_code)]

use mavlink::common::{LOG_DATA_DATA, LOG_ENTRY_DATA, LOG_REQUEST_DATA_DATA, LOG_REQUEST_LIST_DATA, MavMessage};

/// Autopilot family the mock pretends to be. Set by the builder; defaults to PX4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotFamily {
    Px4,
    ArduPilot,
}

/// Replays the FC side of the LOG_* protocol from a fixture byte slice.
pub struct MockLogTransport {
    fixture: Vec<u8>,
    log_id: u16,
    time_utc: u32,
    num_logs_override: Option<u16>,
    drop_offset: Option<u32>,
    multiple_entries: Option<Vec<(u16, u32, u32)>>,
    autopilot: AutopilotFamily,
}

impl MockLogTransport {
    /// Pinned SHA-256 digest of the fixture. Mirrors the constant in
    /// `roz-mavlink/tests/common/mock_log_transport.rs` — if this value
    /// drifts, one of the two mocks has been modified without updating
    /// both fixture-integrity tests.
    pub const FIXTURE_DIGEST_SHA256_HEX: &'static str =
        "ef8e9d125b1494d326ffa97476fd3ddd59df4753634b8ef81f946882dbc044ef";

    /// Default synthetic log id assigned to the fixture entry.
    pub const DEFAULT_LOG_ID: u16 = 1;

    /// Default synthetic UTC timestamp (seconds since epoch) for the fixture entry.
    pub const DEFAULT_TIME_UTC: u32 = 1_719_000_000;

    /// Default FC system/component IDs the mock pretends to be.
    pub const MOCK_TARGET_SYSTEM: u8 = 1;
    pub const MOCK_TARGET_COMPONENT: u8 = 1;

    /// MAVLink LOG_DATA payload carries a fixed 90-byte buffer per upstream spec.
    pub const LOG_DATA_CHUNK_BYTES: usize = 90;

    /// Load the mock from the default fixture path — resolved relative to
    /// this crate's `CARGO_MANIFEST_DIR` and pointing into the sibling
    /// `roz-mavlink` crate's checked-in fixture.
    pub fn load_from_default_fixture() -> std::io::Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let fixture_path = std::path::Path::new(manifest_dir)
            .join("..")
            .join("roz-mavlink")
            .join("tests")
            .join("fixtures")
            .join("px4_sample_session.ulg");
        let fixture = std::fs::read(&fixture_path)?;
        assert!(
            !fixture.is_empty(),
            "fixture at {} is empty — refusing to construct mock",
            fixture_path.display()
        );
        Ok(Self::from_bytes(fixture))
    }

    /// Construct a mock from raw fixture bytes. For synthetic tests that do
    /// not need the checked-in file.
    pub fn from_bytes(fixture: Vec<u8>) -> Self {
        Self {
            fixture,
            log_id: Self::DEFAULT_LOG_ID,
            time_utc: Self::DEFAULT_TIME_UTC,
            num_logs_override: None,
            drop_offset: None,
            multiple_entries: None,
            autopilot: AutopilotFamily::Px4,
        }
    }

    /// On the first `LOG_REQUEST_DATA` that would emit a frame whose `ofs`
    /// equals `ofs`, suppress that frame. The suppression is consumed after
    /// the first hit, so a subsequent `LOG_REQUEST_DATA` covering the same
    /// offset delivers the frame normally.
    #[must_use]
    pub fn with_drop_offset(mut self, ofs: u32) -> Self {
        self.drop_offset = Some(ofs);
        self
    }

    /// Force the `LOG_ENTRY` response's `num_logs` field to 0 (FC reports
    /// no onboard logs — `failure_mode = "no_logs_available"`).
    #[must_use]
    pub fn with_num_logs_zero(mut self) -> Self {
        self.num_logs_override = Some(0);
        self
    }

    /// Force the `LOG_ENTRY` response's `time_utc` field to 0.
    #[must_use]
    pub fn with_time_utc_zero(mut self) -> Self {
        self.time_utc = 0;
        self
    }

    /// Override the default single-entry LOG_ENTRY reply with a sequence of
    /// `(id, time_utc, size)` tuples — one LOG_ENTRY per tuple, in order.
    #[must_use]
    pub fn with_multiple_entries(mut self, entries: Vec<(u16, u32, u32)>) -> Self {
        self.multiple_entries = Some(entries);
        self
    }

    /// Switch the pretended autopilot family. Defaults to PX4.
    #[must_use]
    pub fn with_autopilot(mut self, family: AutopilotFamily) -> Self {
        self.autopilot = family;
        self
    }

    /// Given a client → FC message, produce the FC → client messages the
    /// real FC would emit. Returns an empty vec for messages that do not
    /// trigger an FC response (e.g. `LOG_REQUEST_END`, `HEARTBEAT`).
    pub fn drive_once(&mut self, incoming: &MavMessage) -> Vec<MavMessage> {
        match incoming {
            MavMessage::LOG_REQUEST_LIST(req) => self.handle_log_request_list(req),
            MavMessage::LOG_REQUEST_DATA(req) => self.handle_log_request_data(req),
            _ => Vec::new(),
        }
    }

    /// Raw fixture bytes.
    pub fn fixture_bytes(&self) -> &[u8] {
        &self.fixture
    }

    /// Synthetic log id assigned by the mock.
    pub fn log_id(&self) -> u16 {
        self.log_id
    }

    /// Pretended autopilot family.
    pub fn autopilot(&self) -> AutopilotFamily {
        self.autopilot
    }

    /// Synthetic UTC timestamp (seconds since epoch) assigned by the mock.
    pub fn time_utc(&self) -> u32 {
        self.time_utc
    }

    // -----------------------------------------------------------------------
    // Internal handlers
    // -----------------------------------------------------------------------

    fn handle_log_request_list(&self, _req: &LOG_REQUEST_LIST_DATA) -> Vec<MavMessage> {
        if let Some(entries) = &self.multiple_entries {
            let last_log_num = entries.iter().map(|(id, _, _)| *id).max().unwrap_or(0);
            let num_logs = self
                .num_logs_override
                .unwrap_or_else(|| u16::try_from(entries.len()).unwrap_or(u16::MAX));
            return entries
                .iter()
                .map(|(id, time_utc, size)| {
                    MavMessage::LOG_ENTRY(LOG_ENTRY_DATA {
                        time_utc: *time_utc,
                        size: *size,
                        id: *id,
                        num_logs,
                        last_log_num,
                    })
                })
                .collect();
        }

        let size = u32::try_from(self.fixture.len()).unwrap_or(u32::MAX);
        let num_logs = self.num_logs_override.unwrap_or(1);
        vec![MavMessage::LOG_ENTRY(LOG_ENTRY_DATA {
            time_utc: self.time_utc,
            size,
            id: self.log_id,
            num_logs,
            last_log_num: self.log_id,
        })]
    }

    fn handle_log_request_data(&mut self, req: &LOG_REQUEST_DATA_DATA) -> Vec<MavMessage> {
        if req.id != self.log_id {
            return Vec::new();
        }

        let chunk = Self::LOG_DATA_CHUNK_BYTES;
        let chunk_u32 = u32::try_from(chunk).expect("90 fits in u32");
        let fixture_len_u32 = u32::try_from(self.fixture.len()).unwrap_or(u32::MAX);

        let start = req.ofs;
        if start >= fixture_len_u32 {
            return Vec::new();
        }
        let end = start.saturating_add(req.count).min(fixture_len_u32);

        let mut frames: Vec<MavMessage> = Vec::new();

        let mut cursor = start;
        while cursor < end {
            let remaining_u32 = end - cursor;
            let this_count_u32 = remaining_u32.min(chunk_u32);
            let this_count_usize = usize::try_from(this_count_u32).expect("fits in usize");

            let suppress = self.drop_offset == Some(cursor);
            if suppress {
                self.drop_offset = None;
                cursor += this_count_u32;
                continue;
            }

            let cursor_usize = usize::try_from(cursor).expect("fits in usize");
            let slice_end = cursor_usize + this_count_usize;
            let mut data = [0u8; 90];
            data[..this_count_usize].copy_from_slice(&self.fixture[cursor_usize..slice_end]);

            let count_u8 = u8::try_from(this_count_u32).expect("count <= 90 fits in u8");
            frames.push(MavMessage::LOG_DATA(LOG_DATA_DATA {
                ofs: cursor,
                id: self.log_id,
                count: count_u8,
                data,
            }));

            cursor += this_count_u32;
        }

        if end == fixture_len_u32 && fixture_len_u32 % chunk_u32 == 0 && !frames.is_empty() {
            frames.push(MavMessage::LOG_DATA(LOG_DATA_DATA {
                ofs: end,
                id: self.log_id,
                count: 0,
                data: [0u8; 90],
            }));
        }

        frames
    }
}

impl Default for MockLogTransport {
    fn default() -> Self {
        Self::load_from_default_fixture().expect("default fixture must be present for tests")
    }
}
