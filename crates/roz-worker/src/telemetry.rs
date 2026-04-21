use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use roz_core::messages::TelemetryMsg;
use roz_nats::dispatch::publish_signed;
use roz_nats::subjects::Subjects;
use serde_json::Value;
use uuid::Uuid;

use crate::signing_hooks::WorkerSigningContext;
use crate::telemetry_backpressure::TelemetryBackpressure;
use crate::wal::{DEFAULT_TELEMETRY_BYTE_QUOTA, DEFAULT_TELEMETRY_TTL_SECS, WalStore};

/// Rate-limited telemetry publisher.
///
/// Tracks per-sensor publish timestamps and enforces a maximum publish rate.
/// The actual NATS publishing is deferred to integration; this struct handles
/// rate limiting, subject construction, and message building.
pub struct TelemetryPublisher {
    max_hz: f64,
    last_publish: Mutex<HashMap<String, Instant>>,
}

impl TelemetryPublisher {
    /// Create a new publisher with the given maximum publish rate (Hz).
    pub fn new(max_hz: f64) -> Self {
        Self {
            max_hz,
            last_publish: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether enough time has passed since the last publish for this sensor.
    ///
    /// If `should_publish` returns true, it also updates the last-publish timestamp.
    pub fn should_publish(&self, sensor_name: &str) -> bool {
        if !self.max_hz.is_finite() || self.max_hz <= 0.0 {
            return false;
        }

        let min_interval = std::time::Duration::from_secs_f64(1.0 / self.max_hz);
        let mut map = self.last_publish.lock();
        let now = Instant::now();

        match map.get(sensor_name) {
            Some(last) if now.duration_since(*last) < min_interval => false,
            _ => {
                map.insert(sensor_name.to_string(), now);
                true
            }
        }
    }

    /// Build a `TelemetryMsg` for the given host, sensor, and data.
    pub fn build_message(host_id: &str, sensor: &str, data: Value) -> TelemetryMsg {
        #[allow(clippy::cast_precision_loss)]
        let ts = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        TelemetryMsg {
            ts,
            stream: format!("{host_id}.{sensor}"),
            data,
        }
    }

    /// Build the NATS subject for telemetry using `roz_nats::subjects::Subjects`.
    pub fn subject(host_id: &str, sensor: &str) -> Result<String, roz_core::errors::RozError> {
        Subjects::telemetry(host_id, sensor)
    }
}

/// Publish a telemetry state message to NATS (unsigned legacy path).
///
/// Sends `data` on the `telemetry.{worker_id}.state` subject.
/// Callers are responsible for rate limiting (e.g. via `TelemetryPublisher::should_publish`).
///
/// **Note (Phase 23 FS-04):** Production workers that have enrolled a device
/// signing key should call [`publish_state_signed`] instead. This unsigned
/// variant remains for boot-time paths where the signing key is not yet
/// available, and for pre-v3.0 workers operating under
/// `SIGNED_DISPATCH_ENFORCEMENT=off`.
pub async fn publish_state(nats: &async_nats::Client, worker_id: &str, data: &serde_json::Value) -> anyhow::Result<()> {
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let payload = serde_json::to_vec(data)?;
    nats.publish(subject, payload.into()).await?;
    Ok(())
}

/// Publish a telemetry state message with a `roz-sig-v1` signature header
/// attached (Phase 23 FS-04).
///
/// Computes the signature via `signing_ctx.sign_outbound_worker(correlation_id,
/// payload)` and publishes through `roz_nats::dispatch::publish_signed`. The
/// `correlation_id` is the host's UUID for telemetry — per-host, per-stream
/// correlation matches the server's verifier expectations.
///
/// # Errors
///
/// - Serialization failure on `data`.
/// - Invalid `worker_id` (rejected by `Subjects::telemetry_state`).
/// - Signing failure (missing/corrupt device key → D-09 worker hard-stop; the
///   caller handles that at the top of the publish loop).
/// - NATS transport failure.
pub async fn publish_state_signed(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    worker_id: &str,
    correlation_id: Uuid,
    data: &serde_json::Value,
) -> anyhow::Result<()> {
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let payload = serde_json::to_vec(data)?;
    let header = signing_ctx
        .sign_outbound_worker(correlation_id, &payload)
        .map_err(|e| anyhow::anyhow!("sign telemetry publish: {e}"))?;
    publish_signed(nats, subject, payload, &header)
        .await
        .map_err(|e| anyhow::anyhow!("publish_signed telemetry: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FS-02 store-and-forward fallback (Plan 24-07 Task 1)
// ---------------------------------------------------------------------------

/// Rate-limit the drop-counter log (24-CONTEXT D-07): every 100th drop emits a
/// `tracing::warn!`. One instance is shared worker-wide — the boot wiring
/// instantiates it once and hands it to every caller of
/// [`publish_state_signed_with_buffer`].
pub struct DropCounter {
    drops: AtomicU64,
}

impl DropCounter {
    /// Construct a zeroed counter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            drops: AtomicU64::new(0),
        }
    }

    /// Record one drop. Returns the new total plus a `should_log` hint: log at
    /// `n == 1` and every `n % 100 == 0` thereafter. Per 24-07 must_have
    /// (101 forced evictions → at most 2 log lines).
    pub fn record_and_should_log(&self) -> (u64, bool) {
        let n = self.drops.fetch_add(1, Ordering::Relaxed) + 1;
        (n, n == 1 || n.is_multiple_of(100))
    }
}

impl Default for DropCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of WAL appends between `enforce_fifo_quota` runs (24-RESEARCH
/// §Pitfall 2 batch amortization).
pub const ENFORCE_QUOTA_EVERY: u64 = 64;

/// Compute the buffer-full percentage (0..=100) from `used` / `quota`.
/// Non-positive quota saturates to 100 so an mis-configured worker derates
/// rather than publishes unbounded.
#[must_use]
fn percent_full(used: i64, quota: i64) -> u8 {
    if quota <= 0 {
        return 100;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let pct = ((used as f64 / quota as f64) * 100.0).clamp(0.0, 100.0) as u8;
    pct
}

/// Publish a signed telemetry state frame with WAL store-and-forward fallback
/// (FS-02).
///
/// Behavior:
/// 1. Serialize the payload once.
/// 2. Attempt the signed NATS publish via [`publish_signed`]. On success, return.
/// 3. On NATS publish error, append the raw payload to `telemetry_frames` via
///    [`WalStore::append_telemetry_frame`]. The signed envelope header is NOT
///    persisted — [`crate::telemetry_replay::TelemetryReplay`] re-signs each
///    replayed frame with a fresh correlation_id and a fresh signing sequence
///    number (preserving Phase 23 replay protection).
/// 4. Refresh the backpressure flag via
///    [`TelemetryBackpressure::update`] using the running-total percentage.
/// 5. Every [`ENFORCE_QUOTA_EVERY`] appends, run
///    [`WalStore::enforce_fifo_quota`] with the FS-02 defaults. Eviction log
///    rate is capped by [`DropCounter`] (1/100 per 24-CONTEXT D-07).
///
/// # Errors
///
/// Returns `Err` only on signing or WAL-append failures — either one indicates
/// a condition the caller cannot recover from by retrying NATS. Pure NATS
/// publish errors are absorbed into the fallback path and return `Ok(())`.
#[allow(clippy::too_many_arguments)]
pub async fn publish_state_signed_with_buffer(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    worker_id: &str,
    correlation_id: Uuid,
    data: &serde_json::Value,
    wal: &Arc<WalStore>,
    backpressure: &TelemetryBackpressure,
    drop_counter: &DropCounter,
    append_counter: &AtomicU64,
) -> anyhow::Result<()> {
    // Serialize once — same bytes go to NATS or WAL.
    let payload = serde_json::to_vec(data)?;
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let header = signing_ctx
        .sign_outbound_worker(correlation_id, &payload)
        .map_err(|e| anyhow::anyhow!("sign telemetry publish: {e}"))?;

    match publish_signed(nats, subject, payload.clone(), &header).await {
        Ok(()) => Ok(()),
        Err(nats_err) => {
            // Fallback: buffer the frame. Replay (Plan 24-07 Task 2) re-signs
            // each frame with a fresh correlation + sequence; we do NOT persist
            // the header here.
            let seq = wal
                .append_telemetry_frame(worker_id, "state", &payload)
                .map_err(|e| anyhow::anyhow!("wal append_telemetry_frame: {e}"))?;

            let used = wal
                .telemetry_bytes_used()
                .map_err(|e| anyhow::anyhow!("telemetry_bytes_used: {e}"))?;
            let usage_pct = percent_full(used, DEFAULT_TELEMETRY_BYTE_QUOTA);
            backpressure.update(usage_pct);

            let n = append_counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(ENFORCE_QUOTA_EVERY) {
                let dropped = wal
                    .enforce_fifo_quota(DEFAULT_TELEMETRY_BYTE_QUOTA, DEFAULT_TELEMETRY_TTL_SECS)
                    .map_err(|e| anyhow::anyhow!("enforce_fifo_quota: {e}"))?;
                if dropped > 0 {
                    for _ in 0..dropped {
                        let (total, should_log) = drop_counter.record_and_should_log();
                        if should_log {
                            tracing::warn!(
                                worker_id = %worker_id,
                                total_dropped = total,
                                "telemetry frame dropped under FIFO quota (log cap 1/100)"
                            );
                        }
                    }
                }
            }

            tracing::debug!(
                worker_id = %worker_id,
                seq,
                size = payload.len(),
                used_bytes = used,
                usage_pct,
                nats_error = %nats_err,
                "telemetry buffered to WAL (NATS partitioned)"
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 26-12 OBS-01: protobuf-bytes telemetry publishers (wire-format
// migration)
// ---------------------------------------------------------------------------
//
// These functions are opaque-bytes siblings of the JSON publishers above.
// The caller pre-encodes a `roz.v1.TelemetryUpdate` via
// `prost::Message::encode_to_vec` and passes the bytes in. Subject, signing,
// WAL, backpressure, drop-counter, and `publish_signed` semantics are
// identical to the JSON path — only the serialization step is skipped.
//
// The WAL append path treats the payload as opaque bytes, so
// `telemetry_replay.rs` re-signs stored protobuf frames the same way it
// re-signs the pre-migration JSON frames — no replay-path changes needed.

/// Publish a pre-encoded protobuf telemetry payload (unsigned legacy path).
///
/// Mirrors [`publish_state`] but takes raw bytes instead of `&serde_json::Value`.
/// Production workers that have enrolled a device signing key should call
/// [`publish_state_proto_signed`] or [`publish_state_proto_signed_with_buffer`].
pub async fn publish_state_proto(nats: &async_nats::Client, worker_id: &str, payload: &[u8]) -> anyhow::Result<()> {
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    nats.publish(subject, payload.to_vec().into()).await?;
    Ok(())
}

/// Publish a pre-encoded protobuf telemetry payload signed via `roz-sig-v1`
/// (Phase 23 FS-04).
///
/// Mirrors [`publish_state_signed`] but takes raw bytes.
///
/// # Errors
///
/// - Invalid `worker_id` (rejected by `Subjects::telemetry_state`).
/// - Signing failure (missing/corrupt device key → D-09 worker hard-stop).
/// - NATS transport failure.
pub async fn publish_state_proto_signed(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    worker_id: &str,
    correlation_id: Uuid,
    payload: &[u8],
) -> anyhow::Result<()> {
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let header = signing_ctx
        .sign_outbound_worker(correlation_id, payload)
        .map_err(|e| anyhow::anyhow!("sign telemetry publish: {e}"))?;
    publish_signed(nats, subject, payload.to_vec(), &header)
        .await
        .map_err(|e| anyhow::anyhow!("publish_signed telemetry: {e}"))?;
    Ok(())
}

/// Publish a signed protobuf telemetry frame with WAL store-and-forward
/// fallback (FS-02).
///
/// Mirrors [`publish_state_signed_with_buffer`] but takes raw bytes. The WAL
/// stores the raw payload; `telemetry_replay.rs` treats the bytes as opaque
/// and re-signs + re-publishes verbatim on reconnect — no replay-path
/// changes needed for this migration.
///
/// # Errors
///
/// Same as [`publish_state_signed_with_buffer`]: signing failure or
/// WAL-append failure returns `Err`; pure NATS publish errors are absorbed
/// into the fallback path and return `Ok(())`.
#[allow(clippy::too_many_arguments)]
pub async fn publish_state_proto_signed_with_buffer(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    worker_id: &str,
    correlation_id: Uuid,
    payload: &[u8],
    wal: &Arc<WalStore>,
    backpressure: &TelemetryBackpressure,
    drop_counter: &DropCounter,
    append_counter: &AtomicU64,
) -> anyhow::Result<()> {
    let subject = Subjects::telemetry_state(worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let header = signing_ctx
        .sign_outbound_worker(correlation_id, payload)
        .map_err(|e| anyhow::anyhow!("sign telemetry publish: {e}"))?;

    match publish_signed(nats, subject, payload.to_vec(), &header).await {
        Ok(()) => Ok(()),
        Err(nats_err) => {
            // Fallback: buffer the frame. Replay re-signs each frame with a
            // fresh correlation + sequence; we do NOT persist the header here.
            // WAL treats payload as opaque bytes — protobuf or JSON, identical
            // path.
            let seq = wal
                .append_telemetry_frame(worker_id, "state", payload)
                .map_err(|e| anyhow::anyhow!("wal append_telemetry_frame: {e}"))?;

            let used = wal
                .telemetry_bytes_used()
                .map_err(|e| anyhow::anyhow!("telemetry_bytes_used: {e}"))?;
            let usage_pct = percent_full(used, DEFAULT_TELEMETRY_BYTE_QUOTA);
            backpressure.update(usage_pct);

            let n = append_counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(ENFORCE_QUOTA_EVERY) {
                let dropped = wal
                    .enforce_fifo_quota(DEFAULT_TELEMETRY_BYTE_QUOTA, DEFAULT_TELEMETRY_TTL_SECS)
                    .map_err(|e| anyhow::anyhow!("enforce_fifo_quota: {e}"))?;
                if dropped > 0 {
                    for _ in 0..dropped {
                        let (total, should_log) = drop_counter.record_and_should_log();
                        if should_log {
                            tracing::warn!(
                                worker_id = %worker_id,
                                total_dropped = total,
                                "telemetry frame dropped under FIFO quota (log cap 1/100)"
                            );
                        }
                    }
                }
            }

            tracing::debug!(
                worker_id = %worker_id,
                seq,
                size = payload.len(),
                used_bytes = used,
                usage_pct,
                nats_error = %nats_err,
                "telemetry (proto) buffered to WAL (NATS partitioned)"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use crate::wal::WalStore;
    use ed25519_dalek::SigningKey;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use roz_core::signing::{HEADER_NAME, SignatureEnvelope};
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn rate_limiting_allows_first_publish() {
        let pub_ = TelemetryPublisher::new(10.0);
        assert!(pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_blocks_rapid_second_publish() {
        let pub_ = TelemetryPublisher::new(10.0); // 10 Hz => 100ms interval
        assert!(pub_.should_publish("imu"));
        // Immediately after, should be blocked
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_independent_per_sensor() {
        let pub_ = TelemetryPublisher::new(10.0);
        assert!(pub_.should_publish("imu"));
        // Different sensor should still be allowed
        assert!(pub_.should_publish("gps"));
        // Same sensor should still be blocked
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_allows_after_interval() {
        let pub_ = TelemetryPublisher::new(1000.0); // 1000 Hz => 1ms interval
        assert!(pub_.should_publish("imu"));
        // Sleep just past the interval
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_zero_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(0.0);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_negative_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(-1.0);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_nan_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(f64::NAN);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_infinity_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(f64::INFINITY);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn subject_construction_correct() {
        let subject = TelemetryPublisher::subject("host1", "imu").unwrap();
        assert_eq!(subject, "telemetry.host1.imu");
    }

    #[test]
    fn subject_construction_validates_tokens() {
        let err = TelemetryPublisher::subject("", "imu");
        assert!(err.is_err());
    }

    async fn build_signing_ctx() -> (TempDir, WorkerSigningContext) {
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
        let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);
        (tmp, ctx)
    }

    #[tokio::test]
    async fn publish_state_proto_signed_produces_valid_header_for_payload() {
        // Phase 26-12 OBS-01: prove the signed-header path is wire-format-
        // agnostic by feeding a protobuf-shaped byte pattern instead of
        // serde_json. Mirrors `publish_state_signed_produces_valid_header_for_payload`.
        let (_tmp, ctx) = build_signing_ctx().await;
        let worker_id = "host1";
        let correlation = Uuid::new_v4();
        // A deterministic byte pattern stands in for a real prost-encoded
        // TelemetryUpdate — the signing path does not inspect payload contents.
        let payload: Vec<u8> = vec![0x08, 0x96, 0x01, 0x10, 0x2a, 0x20, 0x01];
        let header = ctx.sign_outbound_worker(correlation, &payload).unwrap();
        let env = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(env.fields.correlation_id, correlation);
        assert_eq!(
            env.fields.payload_hash,
            roz_core::signing::payload_sha256_hex(&payload)
        );
        assert_eq!(
            Subjects::telemetry_state(worker_id).unwrap(),
            format!("telemetry.{worker_id}.state")
        );
        assert_eq!(HEADER_NAME, "roz-sig-v1");
    }

    #[tokio::test]
    async fn publish_state_signed_produces_valid_header_for_payload() {
        // Prove that what publish_state_signed WOULD send on the wire carries
        // a roz-sig-v1 envelope whose payload_hash matches the actual payload
        // bytes. We can't spin up NATS here; instead we reproduce the header
        // construction path and assert the crypto invariants.
        let (_tmp, ctx) = build_signing_ctx().await;
        let worker_id = "host1";
        let correlation = Uuid::new_v4();
        let data = json!({"joints": [1.0, 2.0], "ts": 42});
        let payload = serde_json::to_vec(&data).unwrap();
        let header = ctx.sign_outbound_worker(correlation, &payload).unwrap();
        let env = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(env.fields.correlation_id, correlation);
        // Recomputed payload hash matches.
        assert_eq!(env.fields.payload_hash, roz_core::signing::payload_sha256_hex(&payload));
        // Subject is computable (not part of the signature, but the publish
        // site wires them together).
        assert_eq!(
            Subjects::telemetry_state(worker_id).unwrap(),
            format!("telemetry.{worker_id}.state")
        );
        // Header name matches what roz-nats::dispatch::publish_signed uses.
        assert_eq!(HEADER_NAME, "roz-sig-v1");
    }

    #[test]
    fn percent_full_rounds_down() {
        assert_eq!(percent_full(0, 100), 0);
        assert_eq!(percent_full(50, 100), 50);
        assert_eq!(percent_full(100, 100), 100);
        // Over-quota clamps to 100.
        assert_eq!(percent_full(150, 100), 100);
        assert_eq!(percent_full(90, 100), 90);
        // Zero / negative quota degrades safe (saturates to 100).
        assert_eq!(percent_full(1, 0), 100);
    }

    #[test]
    fn drop_counter_logs_at_1_and_every_100() {
        let dc = DropCounter::new();
        // 1st drop always logs.
        let (n, log) = dc.record_and_should_log();
        assert_eq!(n, 1);
        assert!(log, "first drop should always log");
        // Drops 2..=99 do not log.
        for _ in 0..98 {
            let (_, should) = dc.record_and_should_log();
            assert!(!should);
        }
        // 100th drop logs.
        let (n, log) = dc.record_and_should_log();
        assert_eq!(n, 100);
        assert!(log);
        // 101st drop does not log.
        let (n, log) = dc.record_and_should_log();
        assert_eq!(n, 101);
        assert!(!log);
    }

    #[test]
    fn drop_counter_logs_exactly_twice_for_101_drops() {
        let dc = DropCounter::new();
        let mut log_count = 0u64;
        for _ in 0..101 {
            let (_, should) = dc.record_and_should_log();
            if should {
                log_count += 1;
            }
        }
        // n=1 and n=100 log; 2..=99 and 101 do not. Total = 2.
        assert_eq!(log_count, 2);
    }

    #[test]
    fn enforce_quota_constant_matches_spec() {
        assert_eq!(ENFORCE_QUOTA_EVERY, 64);
    }

    #[test]
    #[allow(clippy::float_cmp)] // serde_json round-trips finite f64 exactly
    fn telemetry_msg_serde_roundtrip() {
        let msg = TelemetryPublisher::build_message("host1", "imu", json!({"x": 1.0}));
        assert_eq!(msg.stream, "host1.imu");
        assert_eq!(msg.data, json!({"x": 1.0}));
        assert!(msg.ts > 0.0);

        // Verify serialization roundtrip
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: TelemetryMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.stream, msg.stream);
        assert_eq!(deserialized.data, msg.data);
        assert_eq!(deserialized.ts, msg.ts);
    }
}
