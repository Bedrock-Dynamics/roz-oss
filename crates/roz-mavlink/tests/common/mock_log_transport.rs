//! File-backed mock MAVLink LOG_* transport replay helper (Phase 26.8, Plan 03).
//!
//! Replays the FC side of the MAVLink log-download protocol against a checked-in
//! PX4 ULG fixture. Not a [`roz_mavlink::backend::MavlinkBackend`]-level transport —
//! this is a pure function over `MavMessage` that Plan 02 unit tests and Plan 08
//! end-to-end integration tests drive by feeding client → FC messages in and
//! reading the FC → client messages back out.
//!
//! ## Protocol model
//!
//! | Client → FC (input)                    | FC → client (output)                                    |
//! |----------------------------------------|---------------------------------------------------------|
//! | [`MavMessage::LOG_REQUEST_LIST`]       | N × [`MavMessage::LOG_ENTRY`] (1 by default)            |
//! | [`MavMessage::LOG_REQUEST_DATA`]       | ceil(count / 90) × [`MavMessage::LOG_DATA`] (+ EOF frame) |
//! | [`MavMessage::LOG_REQUEST_END`]        | (empty — FC releases state silently)                    |
//! | anything else                          | (empty)                                                 |
//!
//! Per MAVLink spec the last `LOG_DATA` frame is either short (`count < 90`)
//! or, when the requested range is an exact multiple of 90, a trailing
//! `count == 0` zero-byte frame.

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
    /// Fixture path relative to the crate root (where `cargo test` runs).
    pub const FIXTURE_PATH: &'static str = "tests/fixtures/px4_sample_session.ulg";

    /// Pinned SHA-256 digest of the fixture. Any byte-level change to the
    /// checked-in fixture trips `fixture_digest_matches_pinned_constant`,
    /// defending against the T-26.8.03-01 tampering threat.
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

    /// Load the mock from the default fixture path. Returns an `io::Error` if
    /// the fixture file is missing or unreadable.
    pub fn load_from_default_fixture() -> std::io::Result<Self> {
        let fixture = std::fs::read(Self::FIXTURE_PATH)?;
        assert!(
            !fixture.is_empty(),
            "fixture at {} is empty — refusing to construct mock",
            Self::FIXTURE_PATH
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
    /// offset delivers the frame normally (simulates the client's gap-retry
    /// path — see Plan 02 D-01 rolling BytesMut reassembly).
    #[must_use]
    pub fn with_drop_offset(mut self, ofs: u32) -> Self {
        self.drop_offset = Some(ofs);
        self
    }

    /// Force the `LOG_ENTRY` response's `num_logs` field to 0, modelling an
    /// FC that reports no onboard logs (Plan 07 `failure_mode = "no_logs_available"`).
    #[must_use]
    pub fn with_num_logs_zero(mut self) -> Self {
        self.num_logs_override = Some(0);
        self
    }

    /// Force the `LOG_ENTRY` response's `time_utc` field to 0, modelling a
    /// pre-GPS-lock FC (Plan 02 D-02 `id`-max tiebreaker path).
    #[must_use]
    pub fn with_time_utc_zero(mut self) -> Self {
        self.time_utc = 0;
        self
    }

    /// Override the default single-entry LOG_ENTRY reply with a sequence of
    /// `(id, time_utc, size)` tuples — one LOG_ENTRY per tuple, in order.
    /// Used by Plan 02 tests exercising the newest-log selection path.
    #[must_use]
    pub fn with_multiple_entries(mut self, entries: Vec<(u16, u32, u32)>) -> Self {
        self.multiple_entries = Some(entries);
        self
    }

    /// Switch the pretended autopilot family. Defaults to PX4; ArduPilot is
    /// useful for exercising Plan 04's `autopilot_kind() != Px4` skip path.
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
            // LOG_REQUEST_END is the client telling the FC to release transfer
            // state; the real FC does not emit a frame in response. Lumped
            // with the catch-all since the observable behaviour (empty vec)
            // is identical — kept here as documentation of the intentional
            // no-op, per MAVLink common.xml §LOG_REQUEST_END (msgid 122).
            _ => Vec::new(),
        }
    }

    /// Raw fixture bytes — the FC's "file on flash".
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
        // Multi-entry override takes precedence over the single-entry default.
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

        // Default single-entry reply from the fixture.
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
        // Mock only knows about its own log id.
        if req.id != self.log_id {
            return Vec::new();
        }

        let chunk = Self::LOG_DATA_CHUNK_BYTES;
        let chunk_u32 = u32::try_from(chunk).expect("90 fits in u32");
        let fixture_len_u32 = u32::try_from(self.fixture.len()).unwrap_or(u32::MAX);

        // Clamp request range to fixture extent.
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

            // Optional drop injection — suppress once, then consume the hook.
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

        // End-of-stream signalling: if the emitted range covered the whole
        // fixture (end == fixture_len) AND the last frame was a full 90-byte
        // chunk (i.e. fixture_len % 90 == 0), append a trailing count=0 frame.
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
    /// Loads the default fixture; panics if the fixture file is missing.
    /// Tests that want explicit error handling use `load_from_default_fixture`.
    fn default() -> Self {
        Self::load_from_default_fixture().expect("default fixture must be present for tests")
    }
}
