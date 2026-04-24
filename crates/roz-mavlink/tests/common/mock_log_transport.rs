//! File-backed mock MAVLink LOG_* transport replay helper (Phase 26.8, Plan 03).
//!
//! Replays the FC side of the MAVLink log-download protocol against a checked-in
//! PX4 ULG fixture. Not a [`roz_mavlink::backend::MavlinkBackend`]-level transport —
//! this is a pure function over `MavMessage` that Plan 02 unit tests and Plan 08
//! end-to-end integration tests drive by feeding client → FC messages in and
//! reading the FC → client messages back out.
//!
//! Stub — implementation lives in Plan 26.8-03 Task 2 GREEN commit.

use mavlink::common::MavMessage;

/// Autopilot family the mock pretends to be.
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
    /// Fixture path relative to the crate root (where `cargo test` runs).
    pub const FIXTURE_PATH: &'static str = "tests/fixtures/px4_sample_session.ulg";

    /// Pinned SHA-256 digest of the fixture. Any byte-level change to the
    /// checked-in fixture trips `fixture_digest_matches_pinned_constant`,
    /// defending against the T-26.8.03-01 tampering threat.
    pub const FIXTURE_DIGEST_SHA256_HEX: &'static str =
        "ef8e9d125b1494d326ffa97476fd3ddd59df4753634b8ef81f946882dbc044ef";

    /// Default synthetic log id assigned to the fixture entry.
    pub const DEFAULT_LOG_ID: u16 = 1;

    /// Default synthetic UTC timestamp (microseconds since epoch) for the fixture entry.
    pub const DEFAULT_TIME_UTC: u32 = 1_719_000_000;

    /// Default FC system/component IDs the mock pretends to be.
    pub const MOCK_TARGET_SYSTEM: u8 = 1;
    pub const MOCK_TARGET_COMPONENT: u8 = 1;

    /// MAVLink LOG_DATA payload carries a fixed 90-byte buffer per upstream spec.
    pub const LOG_DATA_CHUNK_BYTES: usize = 90;

    /// Load the mock from the default fixture path. Implementation: Plan 03 Task 2 GREEN.
    pub fn load_from_default_fixture() -> std::io::Result<Self> {
        let _ = Self::FIXTURE_PATH;
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn from_bytes(_fixture: Vec<u8>) -> Self {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn with_drop_offset(self, _ofs: u32) -> Self {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn with_num_logs_zero(self) -> Self {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn with_time_utc_zero(self) -> Self {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn with_multiple_entries(self, _entries: Vec<(u16, u32, u32)>) -> Self {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    /// Drive the state machine one step. Given a client → FC message, produce
    /// the FC → client messages the real FC would emit. Implementation: Plan
    /// 03 Task 2 GREEN.
    pub fn drive_once(&mut self, _incoming: &MavMessage) -> Vec<MavMessage> {
        unimplemented!("stub — Plan 26.8-03 Task 2 GREEN commit")
    }

    pub fn fixture_bytes(&self) -> &[u8] {
        &self.fixture
    }

    pub fn log_id(&self) -> u16 {
        self.log_id
    }

    pub fn autopilot(&self) -> AutopilotFamily {
        self.autopilot
    }

    pub fn time_utc(&self) -> u32 {
        self.time_utc
    }
}
