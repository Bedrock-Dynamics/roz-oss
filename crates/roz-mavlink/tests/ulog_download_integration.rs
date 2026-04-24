//! Phase 26.8 SC1 + SC7: end-to-end integration tests exercising the
//! [`roz_mavlink::LogDownloader`] state machine against the
//! [`common::mock_log_transport::MockLogTransport`] FC-side replayer and the
//! checked-in PX4 ULG fixture.
//!
//! Each test wires an in-process `mpsc::Sender<MavMessage>` (outbound) + a
//! `broadcast::Sender<MavMessage>` (inbound) pair to a "replay loop" task
//! that drives the mock and fans FC → client messages back to the
//! downloader. This exercises the full 6-message protocol
//! (LOG_REQUEST_LIST → LOG_ENTRY → LOG_REQUEST_DATA → LOG_DATA →
//! LOG_REQUEST_END) without requiring a live UDP transport or a real FC.
//!
//! # Phase 27 SITL swap point
//! The replay loop below is the sole swap point when Phase 27 lands.
//! Replace the `mock.drive_once` dispatch with a forwarder that talks to a
//! real PX4 SITL (`mavlink-router` / `udpin:14540`) and the test assertions
//! remain valid — the checked-in fixture digest can be retargeted via a
//! deliberate update to `FIXTURE_DIGEST_SHA256_HEX`.
//!
//! # Cancellation
//! `replay.abort()` + `drop(in_tx)` is the required teardown — without it
//! the replay task would hold the inbound broadcast channel alive and the
//! downloader's drop-guard `try_send` might observe an unbounded queue.

#![allow(
    clippy::too_many_lines,
    reason = "integration tests carry unavoidable harness scaffolding per roz-server precedent"
)]

mod common;

use std::time::Duration;

use mavlink::common::{LOG_DATA_DATA, LOG_ENTRY_DATA, MavMessage};
use roz_mavlink::{LogDownloader, MAX_LOG_SIZE_BYTES, UlogError};
use sha2::{Digest as _, Sha256};
use tokio::sync::{broadcast, mpsc};

use crate::common::mock_log_transport::MockLogTransport;

// --------------------------------------------------------------------------
// Harness helper: wire the mock into an in-process channel pair.
// --------------------------------------------------------------------------

/// Spawn a replay task that reads client → FC frames from `out_rx`, drives
/// the mock, and broadcasts FC → client frames on `in_tx`. Returns the
/// spawned [`tokio::task::JoinHandle`] so the test can `abort()` at end.
fn spawn_replay(
    mut mock: MockLogTransport,
    mut out_rx: mpsc::Receiver<MavMessage>,
    in_tx: broadcast::Sender<MavMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            for reply in mock.drive_once(&msg) {
                // broadcast send fails when there are no subscribers — that is
                // OK for our happy-path tests because the downloader
                // subscribed before we started driving. Drop silently.
                let _ = in_tx.send(reply);
            }
        }
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest: [u8; 32] = hasher.finalize().into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// --------------------------------------------------------------------------
// Test 1: happy-path full fixture roundtrip
// --------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_full_fixture_roundtrip() {
    let mock = MockLogTransport::load_from_default_fixture().expect("fixture must be present");
    let expected_bytes = mock.fixture_bytes().to_vec();
    let expected_log_id = mock.log_id();

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let (log_id, bytes) = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect("happy-path fixture roundtrip must succeed");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert_eq!(log_id, expected_log_id, "selected log_id must match fixture default");
    assert_eq!(
        bytes.len(),
        expected_bytes.len(),
        "reassembled size must match fixture len"
    );
    assert_eq!(bytes, expected_bytes, "reassembled bytes must equal fixture bytes");
    assert_eq!(
        sha256_hex(&bytes),
        MockLogTransport::FIXTURE_DIGEST_SHA256_HEX,
        "reassembled bytes must match pinned SHA-256 digest"
    );
}

// --------------------------------------------------------------------------
// Test 2: no logs available errors
// --------------------------------------------------------------------------

#[tokio::test]
async fn no_logs_available_errors() {
    let mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture must be present")
        .with_num_logs_zero();

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let err = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect_err("num_logs=0 must surface as NoLogsAvailable");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert!(
        matches!(err, UlogError::NoLogsAvailable),
        "expected NoLogsAvailable, got {err:?}"
    );
}

// --------------------------------------------------------------------------
// Test 3: log list timeout on silent FC + Drop-guard emits LOG_REQUEST_END
// --------------------------------------------------------------------------

#[tokio::test]
async fn log_list_timeout_on_silent_fc() {
    // No replay task: outbound messages are never answered. The downloader
    // must time out waiting for LOG_ENTRY.
    let (out_tx, mut out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());

    let err = downloader
        .fetch_newest(Duration::from_millis(100))
        .await
        .expect_err("silent FC must time out");
    drop(downloader);
    drop(in_tx);

    match err {
        UlogError::LogListTimeout { timeout } => {
            assert_eq!(
                timeout,
                Duration::from_millis(100),
                "timeout field must echo caller's value"
            );
        }
        other => panic!("expected LogListTimeout, got {other:?}"),
    }

    // Drain outbound: expect LOG_REQUEST_LIST + Drop-guard LOG_REQUEST_END.
    let mut saw_list = false;
    let mut saw_end = false;
    while let Ok(msg) = out_rx.try_recv() {
        match msg {
            MavMessage::LOG_REQUEST_LIST(_) => saw_list = true,
            MavMessage::LOG_REQUEST_END(_) => saw_end = true,
            _ => {}
        }
    }
    assert!(saw_list, "downloader must have sent LOG_REQUEST_LIST");
    assert!(saw_end, "Drop-guard must emit LOG_REQUEST_END on error exit");
}

// --------------------------------------------------------------------------
// Test 4: time_utc == 0 triggers id-max tiebreaker (D-02)
// --------------------------------------------------------------------------

#[tokio::test]
async fn time_utc_zero_id_tiebreaker() {
    // All entries have time_utc=0 and identical size; selection must fall
    // back to max(id). Size=0 so no data loop runs.
    let mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture must be present")
        .with_multiple_entries(vec![(5, 0, 0), (9, 0, 0), (7, 0, 0)]);

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let (log_id, bytes) = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect("selection must succeed with 3 entries");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert_eq!(log_id, 9, "all time_utc=0 must fall back to max(id)=9");
    assert!(bytes.is_empty(), "size=0 fixture must return empty payload");
}

// --------------------------------------------------------------------------
// Test 5: time_utc primary selection
// --------------------------------------------------------------------------

#[tokio::test]
async fn time_utc_primary_selection() {
    // Entry 2 has the largest time_utc (200) so it must be selected.
    let mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture must be present")
        .with_multiple_entries(vec![(1, 100, 0), (2, 200, 0), (3, 150, 0)]);

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let (log_id, _bytes) = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect("newest-by-time_utc must succeed");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert_eq!(log_id, 2, "time_utc=200 must win over (100, 150)");
}

// --------------------------------------------------------------------------
// Test 6: gap-retry recovers from a dropped frame
// --------------------------------------------------------------------------

#[tokio::test]
async fn gap_retry_recovers_dropped_frame() {
    // The mock will suppress the first LOG_DATA frame at offset 180 (the
    // third full 90-byte chunk, well inside the fixture). The downloader
    // should notice the gap and re-request, which the mock honors the
    // second time around (the drop is consumed after first hit).
    let mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture must be present")
        .with_drop_offset(180);
    let expected_bytes = mock.fixture_bytes().to_vec();

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let (_log_id, bytes) = downloader
        .fetch_newest(Duration::from_secs(5))
        .await
        .expect("gap-retry must eventually succeed");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert_eq!(
        bytes.len(),
        expected_bytes.len(),
        "recovered length must match fixture"
    );
    assert_eq!(bytes, expected_bytes, "recovered bytes must match fixture");
}

// --------------------------------------------------------------------------
// Test 7: oversized LOG_ENTRY.size rejected before allocation
// --------------------------------------------------------------------------

#[tokio::test]
async fn log_oversized_rejects() {
    // Push a synthetic LOG_ENTRY directly onto the inbound broadcast
    // channel claiming a size > MAX_LOG_SIZE_BYTES. The mock would cap
    // LOG_ENTRY.size to fixture len; bypass the mock for this test by
    // sending the frame manually on the inbound channel.
    let (out_tx, _out_rx_unused) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());

    let oversized = MavMessage::LOG_ENTRY(LOG_ENTRY_DATA {
        time_utc: 1_000_000,
        size: MAX_LOG_SIZE_BYTES + 1,
        id: 42,
        num_logs: 1,
        last_log_num: 0,
    });
    in_tx.send(oversized).expect("send must succeed; subscriber is alive");

    let err = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect_err("oversized size must reject");
    drop(downloader);
    drop(in_tx);

    match err {
        UlogError::LogOversized { size, cap } => {
            assert_eq!(size, MAX_LOG_SIZE_BYTES + 1, "returned size must echo input");
            assert_eq!(cap, MAX_LOG_SIZE_BYTES, "cap must match compiled constant");
        }
        other => panic!("expected LogOversized, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// Test 8: Drop-guard emits LOG_REQUEST_END on early drop (before fetch)
// --------------------------------------------------------------------------

#[tokio::test]
async fn drop_guard_sends_log_request_end_on_early_drop() {
    let (out_tx, mut out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);

    {
        let downloader = LogDownloader::new(out_tx.clone(), in_tx.subscribe());
        // Do NOT call fetch_newest — we want to exercise Drop only.
        drop(downloader);
    }

    // Drop the outer outbound handle so the channel can close after the
    // Drop-guard's try_send leaves its frame in the buffer.
    drop(out_tx);
    drop(in_tx);

    let mut saw_end = false;
    while let Some(msg) = out_rx.recv().await {
        if matches!(msg, MavMessage::LOG_REQUEST_END(_)) {
            saw_end = true;
        }
    }
    assert!(saw_end, "Drop guard must issue LOG_REQUEST_END even without fetch");
}

// --------------------------------------------------------------------------
// Bonus: sanity-check that a direct LOG_DATA frame with count=0 is properly
// recognized as end-of-stream when the fixture length is an exact multiple
// of 90. Exercises RESEARCH pitfall 2 via the integration path.
// --------------------------------------------------------------------------

#[tokio::test]
async fn exact_multiple_of_90_ends_on_zero_count_frame() {
    // Build a synthetic 180-byte fixture (exact multiple of 90). Drive the
    // mock against it; assert the downloader completes cleanly.
    let fixture: Vec<u8> = (0..180u16).map(|n| (n & 0xFF) as u8).collect();
    let mock = MockLogTransport::from_bytes(fixture.clone());

    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(64);
    let (in_tx, _sentinel_rx) = broadcast::channel::<MavMessage>(64);
    let mut downloader = LogDownloader::new(out_tx, in_tx.subscribe());
    let replay = spawn_replay(mock, out_rx, in_tx.clone());

    let (_log_id, bytes) = downloader
        .fetch_newest(Duration::from_secs(2))
        .await
        .expect("exact multiple of 90 fixture must complete");
    drop(downloader);
    drop(in_tx);
    replay.abort();

    assert_eq!(bytes.len(), 180);
    assert_eq!(bytes, fixture);
}

// Sanity check that the unused LOG_DATA_DATA import above is needed for the
// test infrastructure; compile-time check only.
#[allow(dead_code)]
fn _compile_check() -> LOG_DATA_DATA {
    LOG_DATA_DATA {
        ofs: 0,
        id: 0,
        count: 0,
        data: [0u8; 90],
    }
}
