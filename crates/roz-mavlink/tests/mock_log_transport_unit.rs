//! Unit tests for the test-only `MockLogTransport` helper (Phase 26.8, Plan 03).
//!
//! Drives `MockLogTransport::drive_once` through every LOG_* message the
//! upcoming Plan 02 `LogDownloader` state machine will issue, plus the
//! fixture-integrity invariants from the Plan 03 threat model (T-26.8.03-01).
//!
//! Test layout: the helper lives at `tests/common/mock_log_transport.rs` so
//! it can be `mod common;`-included by this file AND by the Plan 08
//! end-to-end integration test without cargo treating it as an extra
//! test binary.

mod common;

use common::mock_log_transport::{AutopilotFamily, MockLogTransport};
use mavlink::common::{
    LOG_DATA_DATA, LOG_ENTRY_DATA, LOG_REQUEST_DATA_DATA, LOG_REQUEST_END_DATA, LOG_REQUEST_LIST_DATA, MavMessage,
};

const FCU_SYS: u8 = 1;
const FCU_COMP: u8 = 1;

fn heartbeat() -> MavMessage {
    use mavlink::common::{HEARTBEAT_DATA, MavAutopilot, MavModeFlag, MavState, MavType};
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GENERIC,
        autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    })
}

fn log_request_list() -> MavMessage {
    MavMessage::LOG_REQUEST_LIST(LOG_REQUEST_LIST_DATA {
        start: 0,
        end: 0xFFFF,
        target_system: FCU_SYS,
        target_component: FCU_COMP,
    })
}

fn log_request_data(id: u16, ofs: u32, count: u32) -> MavMessage {
    MavMessage::LOG_REQUEST_DATA(LOG_REQUEST_DATA_DATA {
        ofs,
        count,
        id,
        target_system: FCU_SYS,
        target_component: FCU_COMP,
    })
}

fn log_request_end() -> MavMessage {
    MavMessage::LOG_REQUEST_END(LOG_REQUEST_END_DATA {
        target_system: FCU_SYS,
        target_component: FCU_COMP,
    })
}

/// Extract all LOG_ENTRY frames from a vec of MavMessages.
fn log_entries(msgs: &[MavMessage]) -> Vec<&LOG_ENTRY_DATA> {
    msgs.iter()
        .filter_map(|m| match m {
            MavMessage::LOG_ENTRY(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// Extract all LOG_DATA frames from a vec of MavMessages.
fn log_data_frames(msgs: &[MavMessage]) -> Vec<&LOG_DATA_DATA> {
    msgs.iter()
        .filter_map(|m| match m {
            MavMessage::LOG_DATA(d) => Some(d),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixture-integrity tests (T-26.8.03-01 mitigation)
// ---------------------------------------------------------------------------

#[test]
fn fixture_file_is_present_and_valid_ulog_magic() {
    let bytes = std::fs::read(MockLogTransport::FIXTURE_PATH)
        .expect("fixture file must be present at tests/fixtures/px4_sample_session.ulg");
    assert!(
        bytes.len() >= 1024 && bytes.len() <= 500 * 1024,
        "fixture size {} out of expected 1 KiB – 500 KiB range",
        bytes.len()
    );
    let expected_magic: [u8; 7] = [0x55, 0x4C, 0x6F, 0x67, 0x01, 0x12, 0x35];
    assert_eq!(
        &bytes[..7],
        &expected_magic,
        "fixture does not start with the PX4 ULog magic header 55 4C 6F 67 01 12 35"
    );
}

#[test]
fn fixture_digest_matches_pinned_constant() {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(MockLogTransport::FIXTURE_PATH).expect("fixture present");
    let digest = Sha256::digest(&bytes);
    let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex,
        MockLogTransport::FIXTURE_DIGEST_SHA256_HEX,
        "fixture sha256 changed; expected Plan 03 to re-pin FIXTURE_DIGEST_SHA256_HEX"
    );
    // Sanity: the pinned constant is a 64-char hex string (not a placeholder).
    assert_eq!(MockLogTransport::FIXTURE_DIGEST_SHA256_HEX.len(), 64);
    assert!(
        MockLogTransport::FIXTURE_DIGEST_SHA256_HEX
            .chars()
            .all(|c| c.is_ascii_hexdigit()),
        "FIXTURE_DIGEST_SHA256_HEX must be a 64-char hex string"
    );
}

// ---------------------------------------------------------------------------
// drive_once — LOG_REQUEST_LIST path
// ---------------------------------------------------------------------------

#[test]
fn drive_once_on_log_request_list_returns_single_log_entry() {
    let mut mock = MockLogTransport::load_from_default_fixture().expect("fixture load");
    let fixture_len = u32::try_from(mock.fixture_bytes().len()).expect("fixture fits in u32");
    let log_id = mock.log_id();
    let time_utc = mock.time_utc();

    let out = mock.drive_once(&log_request_list());
    let entries = log_entries(&out);
    assert_eq!(entries.len(), 1, "expected exactly one LOG_ENTRY");
    let e = entries[0];
    assert_eq!(e.num_logs, 1);
    assert_eq!(e.id, log_id);
    assert_eq!(e.size, fixture_len);
    assert_eq!(e.time_utc, time_utc);
    assert_eq!(
        e.last_log_num, log_id,
        "single-entry mock reports last_log_num == log_id"
    );
    assert_eq!(
        AutopilotFamily::Px4,
        mock.autopilot(),
        "default mock pretends to be PX4"
    );
}

#[test]
fn drive_once_on_log_request_list_num_logs_zero_variant() {
    let mut mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture load")
        .with_num_logs_zero();
    let out = mock.drive_once(&log_request_list());
    let entries = log_entries(&out);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].num_logs, 0, "num_logs-zero knob forces num_logs=0");
}

#[test]
fn drive_once_on_log_request_list_time_utc_zero_variant() {
    let mut mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture load")
        .with_time_utc_zero();
    let out = mock.drive_once(&log_request_list());
    let entries = log_entries(&out);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].time_utc, 0,
        "time_utc-zero knob forces time_utc=0 (pre-GPS-lock FC behaviour)"
    );
}

// ---------------------------------------------------------------------------
// drive_once — LOG_REQUEST_DATA path
// ---------------------------------------------------------------------------

#[test]
fn drive_once_on_log_request_data_returns_chunked_frames() {
    let mut mock = MockLogTransport::load_from_default_fixture().expect("fixture load");
    let fixture_len = u32::try_from(mock.fixture_bytes().len()).expect("fits in u32");
    let log_id = mock.log_id();

    let out = mock.drive_once(&log_request_data(log_id, 0, fixture_len));
    let frames = log_data_frames(&out);

    // Chunk layout per MAVLink common.xml LOG_DATA spec: data[90], count<=90.
    //
    // Either branch of the EOF encoding yields `full + 1` total frames:
    //   - fixture_len % 90 != 0  →  `full` full-90 frames + 1 short final frame (count < 90)
    //   - fixture_len % 90 == 0  →  `full` full-90 frames + 1 trailing zero-byte frame (count == 0)
    let chunk = u32::try_from(MockLogTransport::LOG_DATA_CHUNK_BYTES).expect("90 fits");
    let full = fixture_len / chunk;
    let expected_frames = usize::try_from(full + 1).expect("fits in usize");
    assert_eq!(
        frames.len(),
        expected_frames,
        "expected {expected_frames} LOG_DATA frames for {fixture_len} bytes"
    );

    // Offsets are monotonically increasing by 90.
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.id, log_id, "frame {i} id mismatch");
        assert_eq!(
            f.ofs,
            u32::try_from(i).expect("fits") * chunk,
            "frame {i} offset mismatch",
        );
    }

    // Final frame signals end-of-stream: either short (count < 90) or zero-byte (count == 0).
    let last = frames.last().expect("at least one frame");
    assert!(
        last.count < u8::try_from(MockLogTransport::LOG_DATA_CHUNK_BYTES).expect("90 fits in u8"),
        "final frame MUST be short (count<90) or zero (count=0) to signal EOF; got count={}",
        last.count
    );
}

#[test]
fn drive_once_on_log_request_end_returns_empty() {
    let mut mock = MockLogTransport::load_from_default_fixture().expect("fixture load");
    let out = mock.drive_once(&log_request_end());
    assert!(
        out.is_empty(),
        "FC does not emit a frame in response to LOG_REQUEST_END per MAVLink spec"
    );
}

#[test]
fn drop_offset_suppresses_first_delivery_then_serves_on_retry() {
    let mut mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture load")
        .with_drop_offset(180);
    let fixture_len = u32::try_from(mock.fixture_bytes().len()).expect("fits in u32");
    let log_id = mock.log_id();

    let first = mock.drive_once(&log_request_data(log_id, 0, fixture_len));
    let first_frames = log_data_frames(&first);
    assert!(
        !first_frames.iter().any(|f| f.ofs == 180),
        "first pass must drop the frame at ofs=180"
    );

    // Client detects gap, re-requests starting at 180. Retry serves the dropped frame.
    let retry = mock.drive_once(&log_request_data(log_id, 180, 90));
    let retry_frames = log_data_frames(&retry);
    assert!(
        retry_frames.iter().any(|f| f.ofs == 180),
        "retry request must deliver the previously-dropped frame at ofs=180"
    );
}

// ---------------------------------------------------------------------------
// drive_once — negative / unrelated messages
// ---------------------------------------------------------------------------

#[test]
fn drive_once_on_irrelevant_message_returns_empty() {
    let mut mock = MockLogTransport::load_from_default_fixture().expect("fixture load");
    let out = mock.drive_once(&heartbeat());
    assert!(
        out.is_empty(),
        "mock must not emit any frame in response to HEARTBEAT or other irrelevant messages"
    );
}
