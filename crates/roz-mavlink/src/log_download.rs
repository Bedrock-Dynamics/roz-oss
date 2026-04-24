//! MAVLink LOG_* log-download state machine (Phase 26.8 D-01, D-02).
//!
//! Implements the 6-message protocol LOG_REQUEST_LIST -> LOG_ENTRY ->
//! LOG_REQUEST_DATA -> LOG_DATA -> LOG_REQUEST_END (msgids 117-122)
//! against an inbound `broadcast::Receiver<MavMessage>` + outbound
//! `mpsc::Sender<MavMessage>` pair exposed by [`crate::backend::MavlinkBackend`]
//! (see `backend.rs::subscribe_log_data` / `backend.rs::outbound`).
//!
//! # LOG_REQUEST_END invariant (MANDATORY -- RESEARCH pitfall 1)
//!
//! `impl Drop for LogDownloader` best-effort `try_send`s `LOG_REQUEST_END`
//! on every exit path (success, timeout, error, cancellation). Without
//! this, the FC enters a partial-transfer state and ignores subsequent
//! `LOG_REQUEST_LIST` until reboot. The `end_sent: bool` flag prevents
//! double-send on the explicit success path.
//!
//! # End-of-log detection (RESEARCH pitfall 2)
//!
//! Exit on either (a) LOG_DATA with `count < 90` (short frame) or
//! (b) LOG_DATA with `count == 0` (log size is an exact multiple of 90).
//! Both conditions must be handled -- the protocol does NOT emit a
//! dedicated END frame.
//!
//! # Upstream naming
//!
//! mavlink 0.17.1 generates `MavMessage::LOG_REQUEST_LIST(LOG_REQUEST_LIST_DATA { .. })`
//! etc. -- uniform `<NAME>_DATA` convention confirmed at backend.rs:30-33
//! (`HEARTBEAT_DATA`, `COMMAND_ACK_DATA`, `SET_POSITION_TARGET_LOCAL_NED_DATA`).

use std::time::Duration;

use mavlink::common::{LOG_REQUEST_DATA_DATA, LOG_REQUEST_END_DATA, LOG_REQUEST_LIST_DATA, MavMessage};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};

use crate::transport::{MAV_FCU_COMPONENT_ID, MAV_FCU_SYSTEM_ID};

/// DOS cap for `LOG_ENTRY.size` -- see RESEARCH pitfall 3 (malicious FC
/// claiming `u32::MAX` would OOM the worker on naive `Vec::with_capacity`).
pub const MAX_LOG_SIZE_BYTES: u32 = 500 * 1024 * 1024; // 500 MiB

/// Maximum payload bytes per LOG_DATA frame (upstream fixed `data: [u8; 90]`).
pub const LOG_DATA_MAX_FRAME: u8 = 90;

/// Per-gap retry cap (D-01).
pub const PER_GAP_RETRY_CAP: usize = 3;

/// Total-retry cap across all gaps (RESEARCH pitfall 5).
pub const TOTAL_RETRY_CAP: usize = 10;

/// Errors returned by [`LogDownloader::fetch_newest`].
#[derive(Debug, Error)]
pub enum UlogError {
    #[error("FC reports no logs available")]
    NoLogsAvailable,
    #[error("timed out waiting for LOG_ENTRY after {timeout:?}")]
    LogListTimeout { timeout: Duration },
    #[error("timed out waiting for LOG_DATA; bytes_received={bytes_received}")]
    LogDataTimeout { bytes_received: usize },
    #[error("reassembly retries exhausted at offset {offset}")]
    ReassemblyGapsExhausted { offset: u32, bytes_received: usize },
    #[error("LOG_ENTRY.size {size} exceeds MAX_LOG_SIZE_BYTES ({cap})")]
    LogOversized { size: u32, cap: u32 },
    #[error("outbound channel closed")]
    OutboundClosed,
}

/// D-09 failure_mode taxonomy (stringly-typed for `tracing` structured field).
///
/// Lives at the MAVLink crate boundary so both the state machine and
/// downstream worker-layer glue use one canonical enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    FcUnreachable,
    LogListTimeout,
    LogDataTimeout,
    ReassemblyGapsExhausted,
    DigestMismatch,
    UploadFailed,
    EraseFailed,
    NoLogsAvailable,
    LogOversized,
    AutopilotNotPx4,
}

impl FailureMode {
    /// D-09 canonical string for the `failure_mode` tracing field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FcUnreachable => "fc_unreachable",
            Self::LogListTimeout => "log_list_timeout",
            Self::LogDataTimeout => "log_data_timeout",
            Self::ReassemblyGapsExhausted => "reassembly_gaps_exhausted",
            Self::DigestMismatch => "digest_mismatch",
            Self::UploadFailed => "upload_failed",
            Self::EraseFailed => "erase_failed",
            Self::NoLogsAvailable => "no_logs_available",
            Self::LogOversized => "log_oversized",
            Self::AutopilotNotPx4 => "autopilot_not_px4",
        }
    }
}

/// Stateful MAVLink log-download orchestrator.
///
/// One instance drives one download (`fetch_newest`). Dropping the
/// downloader best-effort issues `LOG_REQUEST_END` on the outbound
/// channel, leaving the FC in a clean state.
pub struct LogDownloader {
    outbound: mpsc::Sender<MavMessage>,
    inbound: broadcast::Receiver<MavMessage>,
    target_system: u8,
    target_component: u8,
    /// Tracks whether a `LOG_REQUEST_END` has already been sent on the
    /// success path. When `false`, the `Drop` impl emits one best-effort.
    /// `pub(crate)` so tests can flip it directly without running the
    /// full protocol.
    pub(crate) end_sent: bool,
}

impl LogDownloader {
    /// Build a new downloader bound to a backend's outbound + inbound
    /// channel pair. Targets the canonical FCU ids (system=1, component=1).
    #[must_use]
    pub fn new(outbound: mpsc::Sender<MavMessage>, inbound: broadcast::Receiver<MavMessage>) -> Self {
        Self {
            outbound,
            inbound,
            target_system: MAV_FCU_SYSTEM_ID,
            target_component: MAV_FCU_COMPONENT_ID,
            end_sent: false,
        }
    }

    /// Issue `LOG_REQUEST_LIST`, collect `LOG_ENTRY` responses, pick the
    /// newest log (D-02 `(time_utc, id)`-max), chunk-request `LOG_DATA`,
    /// reassemble, and return `(log_id, bytes)` on success.
    ///
    /// # Errors
    ///
    /// Returns [`UlogError`] variants for the specific failure mode
    /// (timeout, no logs, oversized, retries exhausted, channel closed).
    /// On every exit path -- Ok or Err -- `LOG_REQUEST_END` is guaranteed
    /// via the `Drop` impl (or the explicit success-path send).
    #[expect(
        clippy::too_many_lines,
        reason = "sequential MAVLink LOG_* protocol state machine; extraction would fragment the 6-step flow"
    )]
    pub async fn fetch_newest(&mut self, timeout: Duration) -> Result<(u16, Vec<u8>), UlogError> {
        // --- Step 1: send LOG_REQUEST_LIST(start=0, end=0xFFFF) ---
        let list_req = MavMessage::LOG_REQUEST_LIST(LOG_REQUEST_LIST_DATA {
            start: 0,
            end: 0xFFFF,
            target_system: self.target_system,
            target_component: self.target_component,
        });
        self.outbound
            .send(list_req)
            .await
            .map_err(|_| UlogError::OutboundClosed)?;

        // --- Step 2: collect LOG_ENTRY responses until num_logs is seen or timeout ---
        // Each tuple: (id, time_utc, size). last_log_num is unused for D-02 selection.
        let mut entries: Vec<(u16, u32, u32)> = Vec::new();
        let mut expected_count: Option<u16> = None;
        let list_deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = list_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(UlogError::LogListTimeout { timeout });
            }
            let recv_fut = self.inbound.recv();
            tokio::select! {
                () = tokio::time::sleep(remaining) => {
                    if entries.is_empty() {
                        return Err(UlogError::LogListTimeout { timeout });
                    }
                    // Partial collection -- proceed with what we have if we
                    // saw at least one entry (num_logs may have been short).
                    break;
                }
                recv_result = recv_fut => {
                    match recv_result {
                        Ok(MavMessage::LOG_ENTRY(entry)) => {
                            if entry.num_logs == 0 {
                                return Err(UlogError::NoLogsAvailable);
                            }
                            expected_count = Some(entry.num_logs);
                            entries.push((entry.id, entry.time_utc, entry.size));
                            if u16::try_from(entries.len()).unwrap_or(u16::MAX) >= entry.num_logs {
                                break;
                            }
                        }
                        // Ignore non-LOG_ENTRY frames during list-collect phase.
                        // Broadcast-lagged is also non-fatal: we re-request
                        // via the retry/timeout path on the data side.
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(UlogError::OutboundClosed);
                        }
                    }
                }
            }
        }

        let _ = expected_count; // documented: informational only, not a hard gate.

        // --- Step 3: pick newest by (time_utc primary, id max tiebreaker) ---
        // D-02: when all time_utc == 0 (pre-GPS-lock FC), fall back to max(id).
        let (log_id, log_size) = entries
            .iter()
            .max_by_key(|(id, time_utc, _size)| (*time_utc, *id))
            .copied()
            .map(|(id, _, size)| (id, size))
            .ok_or(UlogError::NoLogsAvailable)?;

        if entries.iter().all(|(_, t, _)| *t == 0) {
            tracing::warn!(
                log_id,
                "all LOG_ENTRY time_utc == 0; selected newest by max(id) -- FC likely lacked GPS time"
            );
        }

        // --- Step 4: DOS cap before allocation ---
        if log_size > MAX_LOG_SIZE_BYTES {
            return Err(UlogError::LogOversized {
                size: log_size,
                cap: MAX_LOG_SIZE_BYTES,
            });
        }

        // Zero-sized log is a degenerate but valid case -- FC reports a log
        // entry with size 0. Return immediately with empty payload after
        // explicit LOG_REQUEST_END.
        if log_size == 0 {
            let end = MavMessage::LOG_REQUEST_END(LOG_REQUEST_END_DATA {
                target_system: self.target_system,
                target_component: self.target_component,
            });
            self.outbound.send(end).await.map_err(|_| UlogError::OutboundClosed)?;
            self.end_sent = true;
            return Ok((log_id, Vec::new()));
        }

        // --- Step 5: allocate reassembly buffer + received-offset tracking ---
        let mut buf: Vec<u8> = vec![0u8; log_size as usize];
        // received[i] tracks which byte offsets are covered. Dense bitmap is
        // wasteful for 500 MiB; use a sparse Vec<bool> per-offset. For phase-
        // 26.8 correctness we only need to detect gaps, so a coverage Vec is
        // acceptable (500 MiB * 1 byte = 500 MiB worst case -- but typical
        // logs are 10-100 MiB). Keep a simpler byte-level bitmap for clarity.
        let mut received: Vec<bool> = vec![false; log_size as usize];
        let mut bytes_received: usize = 0;
        let mut total_retries: usize = 0;

        // Iteratively request + collect until fully covered.
        let mut per_gap_retries: usize = 0;
        let mut last_requested_ofs: Option<u32> = None;

        loop {
            if bytes_received >= log_size as usize {
                break;
            }

            // Find first missing offset + contiguous missing run length.
            let (first_gap_ofs, gap_len) = find_first_gap(&received, log_size as usize);
            // gap_len >= 1 because bytes_received < log_size => at least one hole.

            // Per-gap retry tracking: if we're re-requesting the same offset
            // for the third time, give up on this gap and surface the error.
            if last_requested_ofs == Some(first_gap_ofs) {
                per_gap_retries = per_gap_retries.saturating_add(1);
            } else {
                per_gap_retries = 0;
            }
            if per_gap_retries >= PER_GAP_RETRY_CAP || total_retries >= TOTAL_RETRY_CAP {
                return Err(UlogError::ReassemblyGapsExhausted {
                    offset: first_gap_ofs,
                    bytes_received,
                });
            }

            // --- Step 6: send LOG_REQUEST_DATA for the first gap ---
            // count is u32 per LOG_REQUEST_DATA_DATA field type.
            let req = MavMessage::LOG_REQUEST_DATA(LOG_REQUEST_DATA_DATA {
                ofs: first_gap_ofs,
                count: u32::try_from(gap_len).unwrap_or(u32::MAX),
                id: log_id,
                target_system: self.target_system,
                target_component: self.target_component,
            });
            self.outbound.send(req).await.map_err(|_| UlogError::OutboundClosed)?;
            last_requested_ofs = Some(first_gap_ofs);

            // --- Step 7: receive LOG_DATA frames for this request ---
            let data_deadline = tokio::time::Instant::now() + timeout;
            let mut got_any_this_round = false;
            let mut stream_ended = false;

            loop {
                let remaining = data_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let recv_fut = self.inbound.recv();
                let msg_opt = tokio::select! {
                    () = tokio::time::sleep(remaining) => None,
                    r = recv_fut => Some(r),
                };
                let Some(recv_result) = msg_opt else { break };

                match recv_result {
                    Ok(MavMessage::LOG_DATA(frame)) => {
                        // RESEARCH pitfall 4: drop frames whose id != our log_id.
                        if frame.id != log_id {
                            continue;
                        }
                        // Zero-byte frame signals end-of-log when size is an
                        // exact multiple of 90 (RESEARCH pitfall 2).
                        if frame.count == 0 {
                            stream_ended = true;
                            break;
                        }
                        let ofs = frame.ofs as usize;
                        let count = frame.count as usize;
                        // Arithmetic bounds check (T-26.8.02-04): reject
                        // frames that would write past the declared log size.
                        if ofs.checked_add(count).is_none_or(|end| end > buf.len()) {
                            // Malformed frame; drop silently.
                            continue;
                        }
                        buf[ofs..ofs + count].copy_from_slice(&frame.data[..count]);
                        // Count only newly-covered bytes to avoid double-counting.
                        let mut newly_covered = 0usize;
                        for byte_ofs in ofs..ofs + count {
                            if !received[byte_ofs] {
                                received[byte_ofs] = true;
                                newly_covered += 1;
                            }
                        }
                        bytes_received = bytes_received.saturating_add(newly_covered);
                        got_any_this_round = true;
                        // Short frame signals end-of-log.
                        if count < LOG_DATA_MAX_FRAME as usize {
                            stream_ended = true;
                            break;
                        }
                        // Keep going until we hit the request's window end
                        // or the deadline -- no explicit window exit here
                        // because we rely on bytes_received to drive the
                        // outer loop.
                        if bytes_received >= buf.len() {
                            break;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(UlogError::OutboundClosed);
                    }
                }
            }

            if stream_ended {
                // End-of-log seen; if we have gaps before the short/zero
                // frame, loop around to re-request them.
                if bytes_received >= buf.len() {
                    break;
                }
                // Re-request remaining gaps on the next iteration.
                total_retries = total_retries.saturating_add(1);
                continue;
            }

            if !got_any_this_round {
                // Timeout with nothing received: count toward retry caps.
                total_retries = total_retries.saturating_add(1);
                if total_retries >= TOTAL_RETRY_CAP {
                    return Err(UlogError::LogDataTimeout { bytes_received });
                }
                // Will re-request (same or advanced gap) next iteration.
                continue;
            }

            // Progress happened; count a retry only if we still have gaps.
            if bytes_received < buf.len() {
                total_retries = total_retries.saturating_add(1);
            }
        }

        // Truncate buf to bytes_received if the FC ended early with gaps --
        // but only if we exited via the end-of-stream path AND have no gaps
        // up to some prefix. For Phase 26.8 simplicity we return the fully-
        // allocated buffer when bytes_received == log_size, and surface an
        // error otherwise (handled above via ReassemblyGapsExhausted).
        if bytes_received < buf.len() {
            // This shouldn't be reachable due to above retry-cap guards,
            // but be defensive.
            return Err(UlogError::ReassemblyGapsExhausted {
                offset: find_first_gap(&received, buf.len()).0,
                bytes_received,
            });
        }

        // --- Step 8: explicit LOG_REQUEST_END on success path ---
        let end = MavMessage::LOG_REQUEST_END(LOG_REQUEST_END_DATA {
            target_system: self.target_system,
            target_component: self.target_component,
        });
        self.outbound.send(end).await.map_err(|_| UlogError::OutboundClosed)?;
        self.end_sent = true;

        Ok((log_id, buf))
    }
}

impl Drop for LogDownloader {
    fn drop(&mut self) {
        if self.end_sent {
            return;
        }
        let msg = MavMessage::LOG_REQUEST_END(LOG_REQUEST_END_DATA {
            target_system: self.target_system,
            target_component: self.target_component,
        });
        // try_send: Drop runs in sync context; outbound mpsc may be full.
        // Best-effort; if it fails the FC recovers on reboot.
        let _ = self.outbound.try_send(msg);
    }
}

/// Return `(offset, length)` of the first contiguous run of `false` bytes
/// in `received`. Callers must only invoke when `bytes_received < buf.len()`.
fn find_first_gap(received: &[bool], total: usize) -> (u32, usize) {
    let mut i = 0usize;
    while i < total && received[i] {
        i += 1;
    }
    let start = i;
    while i < total && !received[i] {
        i += 1;
    }
    let len = i - start;
    (u32::try_from(start).unwrap_or(u32::MAX), len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mavlink::common::{LOG_DATA_DATA, LOG_ENTRY_DATA};
    use tokio::sync::{broadcast, mpsc};

    /// Build a fresh outbound mpsc + inbound broadcast pair + downloader.
    fn make_harness() -> (mpsc::Receiver<MavMessage>, broadcast::Sender<MavMessage>, LogDownloader) {
        let (outbound_tx, outbound_rx) = mpsc::channel::<MavMessage>(32);
        let (inbound_tx, inbound_rx) = broadcast::channel::<MavMessage>(64);
        let downloader = LogDownloader::new(outbound_tx, inbound_rx);
        (outbound_rx, inbound_tx, downloader)
    }

    fn log_entry(id: u16, time_utc: u32, size: u32, num_logs: u16) -> MavMessage {
        MavMessage::LOG_ENTRY(LOG_ENTRY_DATA {
            time_utc,
            size,
            id,
            num_logs,
            last_log_num: num_logs.saturating_sub(1),
        })
    }

    fn log_data_frame(id: u16, ofs: u32, payload: &[u8]) -> MavMessage {
        let mut data = [0u8; 90];
        let copy_len = payload.len().min(90);
        data[..copy_len].copy_from_slice(&payload[..copy_len]);
        MavMessage::LOG_DATA(LOG_DATA_DATA {
            ofs,
            id,
            count: u8::try_from(copy_len).unwrap_or(90),
            data,
        })
    }

    #[tokio::test]
    async fn new_downloader_does_not_send_end_on_drop_with_end_sent_true() {
        let (mut outbound_rx, _in_tx, mut downloader) = make_harness();
        downloader.end_sent = true;
        drop(downloader);
        assert!(
            outbound_rx.try_recv().is_err(),
            "end_sent=true must suppress LOG_REQUEST_END on drop"
        );
    }

    #[tokio::test]
    async fn drop_without_end_sent_issues_log_request_end() {
        let (mut outbound_rx, _in_tx, downloader) = make_harness();
        drop(downloader);
        let msg = outbound_rx.try_recv().expect("Drop must emit LOG_REQUEST_END");
        assert!(
            matches!(msg, MavMessage::LOG_REQUEST_END(_)),
            "Drop must emit LOG_REQUEST_END, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn fetch_newest_selects_by_time_utc_primary() {
        let (mut outbound_rx, in_tx, mut downloader) = make_harness();

        // Seed 3 entries before starting fetch (broadcast buffers). Log
        // size = 0 so fetch_newest returns after selection.
        in_tx.send(log_entry(1, 100, 0, 3)).unwrap();
        in_tx.send(log_entry(2, 200, 0, 3)).unwrap();
        in_tx.send(log_entry(3, 150, 0, 3)).unwrap();

        let result = downloader.fetch_newest(Duration::from_millis(500)).await;
        let (log_id, bytes) = result.expect("should select newest");
        assert_eq!(log_id, 2, "time_utc=200 wins, which is id=2");
        assert!(bytes.is_empty(), "size=0 returns empty payload");

        // Drain outbound: LOG_REQUEST_LIST + LOG_REQUEST_END (explicit,
        // since size=0 short-circuits the data loop).
        let mut saw_list = false;
        let mut saw_end = false;
        while let Ok(msg) = outbound_rx.try_recv() {
            match msg {
                MavMessage::LOG_REQUEST_LIST(_) => saw_list = true,
                MavMessage::LOG_REQUEST_END(_) => saw_end = true,
                _ => {}
            }
        }
        assert!(saw_list, "should have sent LOG_REQUEST_LIST");
        assert!(saw_end, "should have sent LOG_REQUEST_END");
    }

    #[tokio::test]
    async fn fetch_newest_selects_by_id_max_when_time_utc_zero() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        in_tx.send(log_entry(5, 0, 0, 3)).unwrap();
        in_tx.send(log_entry(9, 0, 0, 3)).unwrap();
        in_tx.send(log_entry(7, 0, 0, 3)).unwrap();

        let (log_id, _bytes) = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect("should select newest");
        assert_eq!(log_id, 9, "all time_utc=0 => max(id) tiebreaker = 9");
    }

    #[tokio::test]
    async fn fetch_newest_errors_on_log_entry_num_logs_zero() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        in_tx.send(log_entry(0, 0, 0, 0)).unwrap();

        let err = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect_err("num_logs=0 must error");
        assert!(matches!(err, UlogError::NoLogsAvailable), "got {err:?}");
    }

    #[tokio::test]
    async fn fetch_newest_errors_on_log_oversized() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        // Single entry with size > cap.
        in_tx.send(log_entry(1, 100, MAX_LOG_SIZE_BYTES + 1, 1)).unwrap();

        let err = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect_err("oversized size must error");
        match err {
            UlogError::LogOversized { size, cap } => {
                assert_eq!(size, MAX_LOG_SIZE_BYTES + 1);
                assert_eq!(cap, MAX_LOG_SIZE_BYTES);
            }
            other => panic!("expected LogOversized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_data_short_frame_ends_stream() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        // Log of 135 bytes: 1 full frame (90) + 1 short frame (45).
        in_tx.send(log_entry(7, 100, 135, 1)).unwrap();
        let first_payload: Vec<u8> = (0..90u8).collect();
        let second_payload: Vec<u8> = (90..135u8).collect();
        in_tx.send(log_data_frame(7, 0, &first_payload)).unwrap();
        in_tx.send(log_data_frame(7, 90, &second_payload)).unwrap();

        let (log_id, bytes) = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect("short frame should complete the download");
        assert_eq!(log_id, 7);
        assert_eq!(bytes.len(), 135);
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[89], 89);
        assert_eq!(bytes[134], 134);
    }

    #[tokio::test]
    async fn log_data_zero_count_ends_exact_multiple() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        // Log of 180 bytes (exact multiple of 90): 2 full frames + zero frame.
        in_tx.send(log_entry(7, 100, 180, 1)).unwrap();
        let first_payload: Vec<u8> = (0..90u8).collect();
        let second_payload: Vec<u8> = (90..180u8).collect();
        in_tx.send(log_data_frame(7, 0, &first_payload)).unwrap();
        in_tx.send(log_data_frame(7, 90, &second_payload)).unwrap();
        // Zero-byte frame to signal end-of-stream.
        in_tx
            .send(MavMessage::LOG_DATA(LOG_DATA_DATA {
                ofs: 180,
                id: 7,
                count: 0,
                data: [0u8; 90],
            }))
            .unwrap();

        let (log_id, bytes) = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect("zero-byte frame should complete the download");
        assert_eq!(log_id, 7);
        assert_eq!(bytes.len(), 180);
        assert_eq!(bytes[179], 179);
    }

    #[tokio::test]
    async fn unsolicited_log_data_with_mismatched_id_dropped() {
        let (_outbound_rx, in_tx, mut downloader) = make_harness();

        // Log of 45 bytes under id=5; an unsolicited id=99 frame arrives
        // first and must be silently dropped.
        in_tx.send(log_entry(5, 100, 45, 1)).unwrap();
        let unsolicited: Vec<u8> = vec![0xAAu8; 45];
        in_tx.send(log_data_frame(99, 0, &unsolicited)).unwrap();
        let real: Vec<u8> = (0..45u8).collect();
        in_tx.send(log_data_frame(5, 0, &real)).unwrap();

        let (log_id, bytes) = downloader
            .fetch_newest(Duration::from_millis(500))
            .await
            .expect("legitimate frame should complete");
        assert_eq!(log_id, 5);
        assert_eq!(bytes.len(), 45);
        // Must match the real payload (0..45), NOT the unsolicited 0xAA.
        assert_eq!(bytes, real);
    }

    #[test]
    fn failure_mode_as_str_covers_all_variants() {
        assert_eq!(FailureMode::FcUnreachable.as_str(), "fc_unreachable");
        assert_eq!(FailureMode::LogListTimeout.as_str(), "log_list_timeout");
        assert_eq!(FailureMode::LogDataTimeout.as_str(), "log_data_timeout");
        assert_eq!(
            FailureMode::ReassemblyGapsExhausted.as_str(),
            "reassembly_gaps_exhausted"
        );
        assert_eq!(FailureMode::DigestMismatch.as_str(), "digest_mismatch");
        assert_eq!(FailureMode::UploadFailed.as_str(), "upload_failed");
        assert_eq!(FailureMode::EraseFailed.as_str(), "erase_failed");
        assert_eq!(FailureMode::NoLogsAvailable.as_str(), "no_logs_available");
        assert_eq!(FailureMode::LogOversized.as_str(), "log_oversized");
        assert_eq!(FailureMode::AutopilotNotPx4.as_str(), "autopilot_not_px4");
    }
}
