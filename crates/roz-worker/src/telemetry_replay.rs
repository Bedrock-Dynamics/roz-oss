//! Telemetry replay loop for the FS-02 store-and-forward buffer (Plan 24-07).
//!
//! RED stub — Task 2 tests fail via `todo!()` until GREEN lands.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

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
    #[must_use]
    pub const fn new(wal: Arc<WalStore>, signing_ctx: Arc<WorkerSigningContext>) -> Self {
        Self { wal, signing_ctx }
    }

    #[allow(clippy::unused_async)]
    pub async fn run_once(&self, _nats: &async_nats::Client, _worker_id: &str) -> anyhow::Result<usize> {
        todo!("Plan 24-07 Task 2 GREEN: drain WAL buffer into NATS")
    }

    pub(crate) fn compute_delay(&self, _ts_str: &str, _now: DateTime<Utc>) -> anyhow::Result<Duration> {
        todo!("Plan 24-07 Task 2 GREEN: compute rate-limited delay per age band")
    }

    // Expose wal for the empty-buffer test.
    pub(crate) fn wal(&self) -> &Arc<WalStore> {
        &self.wal
    }

    // Silence unused-field warning until GREEN wires the signing ctx.
    #[allow(dead_code)]
    fn signing_ctx(&self) -> &Arc<WorkerSigningContext> {
        &self.signing_ctx
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
    use uuid::Uuid;

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
