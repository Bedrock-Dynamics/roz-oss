//! Telemetry replay loop for the FS-02 store-and-forward buffer (Plan 24-07).
//!
//! On NATS reconnect, call [`TelemetryReplay::run_once`] to drain
//! [`WalStore::list_unacked_telemetry`]:
//!
//! - Backlog ages < [`LIVE_REPLAY_CUTOFF_SECS`] → replay at ~original rate
//!   (no aggressive acceleration).
//! - Backlog ages ≥ [`LIVE_REPLAY_CUTOFF_SECS`] → replay at up to
//!   [`REPLAY_SPEEDUP_FACTOR`]× the source rate, capped at 500 Hz
//!   (the [`MIN_REPLAY_INTERVAL_MS`] floor; 24-CONTEXT D-06).
//!
//! Every replayed frame is re-signed with a FRESH correlation_id via
//! [`WorkerSigningContext::sign_outbound_worker`], which also allocates a
//! fresh monotonic signing sequence number through the WAL. That preserves
//! the Phase 23 replay-protection linearity across partition boundaries —
//! the server's dedup path (Plan 24-07 Task 3) compares against
//! `SignedFields::sequence_number`, not against the original buffered
//! `telemetry_frames.seq`.
//!
//! Jitter budget per 24-CONTEXT D-06: < 10 ms per frame.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use roz_nats::dispatch::publish_signed;
use roz_nats::subjects::Subjects;
use uuid::Uuid;

use crate::signing_hooks::WorkerSigningContext;
use crate::wal::WalStore;

/// 500 Hz ceiling → minimum inter-frame delay of 2 ms.
pub const MIN_REPLAY_INTERVAL_MS: u64 = 2;

/// Age cutoff (seconds) separating "live" replay pacing from accelerated
/// catch-up pacing.
pub const LIVE_REPLAY_CUTOFF_SECS: i64 = 5;

/// Accelerator applied to the base inter-frame delay when the backlog is
/// older than [`LIVE_REPLAY_CUTOFF_SECS`].
pub const REPLAY_SPEEDUP_FACTOR: u32 = 10;

/// Base inter-frame delay at nominal 100 Hz telemetry = 10 ms.
const BASE_INTERVAL_MS: u64 = 10;

/// Worker-side reconnect drain for the FS-02 telemetry buffer.
pub struct TelemetryReplay {
    wal: Arc<WalStore>,
    signing_ctx: Arc<WorkerSigningContext>,
}

impl TelemetryReplay {
    /// Construct a replay driver. Both handles are `Arc`-cloned so the caller
    /// retains ownership for other subsystems.
    #[must_use]
    pub const fn new(wal: Arc<WalStore>, signing_ctx: Arc<WorkerSigningContext>) -> Self {
        Self { wal, signing_ctx }
    }

    /// Drain the WAL buffer into NATS, returning the number of frames
    /// successfully replayed. Stops at the first publish failure: frames
    /// before the failure are acked; frames after remain unacked for the
    /// next reconnect.
    ///
    /// # Errors
    ///
    /// Returns `Err` on WAL read failures, timestamp-parse failures, or
    /// signing failures. Pure NATS publish errors are absorbed: the caller
    /// still observes `Ok(n)` where `n` is the number of frames drained
    /// before the first failure.
    pub async fn run_once(&self, nats: &async_nats::Client, worker_id: &str) -> anyhow::Result<usize> {
        let frames = self
            .wal
            .list_unacked_telemetry()
            .map_err(|e| anyhow::anyhow!("list_unacked_telemetry: {e}"))?;
        if frames.is_empty() {
            return Ok(0);
        }
        let now = Utc::now();
        let mut last_ack = 0i64;
        let mut replayed = 0usize;
        let total = frames.len();

        for (i, (seq, _w, ts_str, frame_type, payload)) in frames.iter().enumerate() {
            let subject = match frame_type.as_str() {
                "state" => Subjects::telemetry_state(worker_id),
                "sensors" => Subjects::telemetry_sensors(worker_id),
                other => Subjects::telemetry(worker_id, other),
            }
            .map_err(|e| anyhow::anyhow!("invalid subject: {e}"))?;

            let delay = self.compute_delay(ts_str, now)?;
            if delay.as_millis() > 0 {
                tokio::time::sleep(delay).await;
            }

            // Fresh correlation_id per replay — the WAL-allocated signing seq
            // inside the envelope keeps Phase 23 replay-protection linear
            // across outages. The server's dedup map (Plan 24-07 Task 3)
            // compares against `SignedFields::sequence_number`, not the
            // original buffered `telemetry_frames.seq`.
            let correlation = Uuid::new_v4();
            let header = self
                .signing_ctx
                .sign_outbound_worker(correlation, payload)
                .map_err(|e| anyhow::anyhow!("re-sign replay frame: {e}"))?;
            if let Err(e) = publish_signed(nats, subject, payload.clone(), &header).await {
                tracing::warn!(
                    error = %e,
                    replayed,
                    remaining = total - i,
                    "telemetry replay publish failed; stopping drain"
                );
                break;
            }
            last_ack = *seq;
            replayed += 1;
        }

        if last_ack > 0
            && let Err(e) = self.wal.ack_telemetry_up_to(last_ack)
        {
            tracing::error!(error = %e, last_ack, "failed to ack replayed frames");
        }
        Ok(replayed)
    }

    /// Per-frame pacing delay based on backlog age. Live (< 5 s) pacing uses
    /// the nominal 10 ms base; stale (≥ 5 s) pacing accelerates 10× but never
    /// below [`MIN_REPLAY_INTERVAL_MS`] (500 Hz ceiling, 24-CONTEXT D-06).
    pub(crate) fn compute_delay(&self, ts_str: &str, now: DateTime<Utc>) -> anyhow::Result<Duration> {
        let ts = DateTime::parse_from_rfc3339(ts_str)
            .map_err(|e| anyhow::anyhow!("parse rfc3339: {e}"))?
            .with_timezone(&Utc);
        let age_secs = (now - ts).num_seconds().max(0);
        if age_secs < LIVE_REPLAY_CUTOFF_SECS {
            Ok(Duration::from_millis(BASE_INTERVAL_MS))
        } else {
            let sped = BASE_INTERVAL_MS / u64::from(REPLAY_SPEEDUP_FACTOR);
            Ok(Duration::from_millis(sped.max(MIN_REPLAY_INTERVAL_MS)))
        }
    }

    /// Test-only accessor for the underlying WAL store. Exposed for the
    /// empty-buffer contract test in this module.
    #[cfg(test)]
    pub(crate) fn wal(&self) -> &Arc<WalStore> {
        &self.wal
    }
}

/// Reconnect-driven replay loop. Drives [`TelemetryReplay::run_once`] each
/// time `reconnect_notify` fires. Plan 24-09 wires the caller (worker main).
///
/// # Errors
/// Propagates `Err` from `run_once`. Cancellation returns `Ok(())`.
pub async fn run_telemetry_replay(
    replay: Arc<TelemetryReplay>,
    nats: async_nats::Client,
    worker_id: String,
    mut reconnect_notify: tokio::sync::mpsc::Receiver<()>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            Some(()) = reconnect_notify.recv() => {
                match replay.run_once(&nats, &worker_id).await {
                    Ok(n) => tracing::info!(replayed = n, "telemetry replay completed"),
                    Err(e) => tracing::error!(error = %e, "telemetry replay failed"),
                }
            }
            () = cancel.cancelled() => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use ed25519_dalek::SigningKey;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use tempfile::TempDir;

    async fn fixture() -> (TempDir, Arc<TelemetryReplay>) {
        let tmp = TempDir::new().unwrap();
        let provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        let server_signing = SigningKey::from_bytes(&[9u8; 32]);
        let svk_bytes = server_signing.verifying_key().to_bytes();
        save(tmp.path(), &provider, tenant, 1, &[7u8; 32], &svk_bytes)
            .await
            .unwrap();
        let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();
        let wal = Arc::new(WalStore::open(":memory:").unwrap());
        let ctx = Arc::new(WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal.clone()));
        let replay = Arc::new(TelemetryReplay::new(wal, ctx));
        (tmp, replay)
    }

    #[tokio::test]
    async fn replay_empty_buffer_is_noop() {
        let (_tmp, replay) = fixture().await;
        // Empty WAL → list returns nothing. run_once full path is covered by
        // Plan 24-09's toxiproxy suite (live NATS client required).
        let frames = replay.wal().list_unacked_telemetry().unwrap();
        assert!(frames.is_empty());
    }

    #[tokio::test]
    async fn compute_delay_live_backlog_uses_base() {
        let (_tmp, replay) = fixture().await;
        let now = Utc::now();
        let fresh = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let d = replay.compute_delay(&fresh, now).unwrap();
        assert_eq!(d, Duration::from_millis(BASE_INTERVAL_MS));
    }

    #[tokio::test]
    async fn compute_delay_stale_backlog_accelerates_to_cap() {
        let (_tmp, replay) = fixture().await;
        let now = Utc::now();
        let stale = (now - chrono::Duration::seconds(30)).to_rfc3339();
        let d = replay.compute_delay(&stale, now).unwrap();
        assert_eq!(d, Duration::from_millis(MIN_REPLAY_INTERVAL_MS));
    }

    #[tokio::test]
    async fn compute_delay_boundary_5_seconds_is_stale() {
        let (_tmp, replay) = fixture().await;
        let now = Utc::now();
        let edge = (now - chrono::Duration::seconds(5)).to_rfc3339();
        let d = replay.compute_delay(&edge, now).unwrap();
        assert_eq!(d, Duration::from_millis(MIN_REPLAY_INTERVAL_MS));
    }

    #[test]
    fn constants_match_spec() {
        assert_eq!(MIN_REPLAY_INTERVAL_MS, 2);
        assert_eq!(LIVE_REPLAY_CUTOFF_SECS, 5);
        assert_eq!(REPLAY_SPEEDUP_FACTOR, 10);
    }
}
