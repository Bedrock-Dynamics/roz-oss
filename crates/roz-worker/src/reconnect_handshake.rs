//! Reconnect handshake publisher (Phase 24 FS-03 D-10).
//!
//! After NATS reconnect, the worker calls [`publish_worker_online`] to signal
//! the server that it has buffered state. The server replies on
//! `roz.tasks.{worker_id}` with per-task [`roz_core::reconnect::ResumeInstruction`]s.
//!
//! Wire types live in [`roz_core::reconnect`] — this module only provides the
//! worker-side signed-publish helper. Duplicate definitions here would be a
//! regression (24-PATTERNS §Pattern 5).
//!
//! Plan 24-09 wires [`publish_worker_online`] into `main.rs` after NATS
//! reconnect and spawns the `roz.tasks.{worker_id}` subscriber.

use roz_core::reconnect::WorkerOnlineSnapshot;
use roz_nats::Subjects;
use roz_nats::dispatch::publish_signed;
use uuid::Uuid;

use crate::signing_hooks::WorkerSigningContext;

/// Publish the worker-online snapshot via the Phase 23 signed envelope.
///
/// Serializes `snapshot` as JSON, signs with
/// [`WorkerSigningContext::sign_outbound_worker`], and publishes on
/// [`Subjects::state_worker_online`]. A fresh correlation id is generated
/// per publish — the server uses it purely for log correlation; the
/// envelope's sequence number + payload hash are what the signing gate
/// verifies.
///
/// # Errors
///
/// - Serialization failure (structurally impossible for well-formed
///   [`WorkerOnlineSnapshot`]).
/// - Signing failure (WAL I/O or canonicalization).
/// - NATS transport failure.
pub async fn publish_worker_online(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    snapshot: &WorkerOnlineSnapshot,
) -> anyhow::Result<()> {
    let subject = Subjects::state_worker_online().to_string();
    let payload = serde_json::to_vec(snapshot)?;
    let correlation = Uuid::new_v4();
    let header = signing_ctx
        .sign_outbound_worker(correlation, &payload)
        .map_err(|e| anyhow::anyhow!("sign worker-online: {e}"))?;
    publish_signed(nats, subject, payload, &header)
        .await
        .map_err(|e| anyhow::anyhow!("publish worker-online: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use crate::wal::WalStore;
    use ed25519_dalek::SigningKey;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use roz_core::reconnect::TaskProgress;
    use roz_core::signing::{Direction, SignatureEnvelope, payload_sha256_hex};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn build_ctx() -> (TempDir, WorkerSigningContext) {
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
        (tmp, WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal))
    }

    /// Verify the signed-envelope construction path mirrors `publish_state_signed`:
    /// direction=WorkerToServer, payload_hash matches the snapshot bytes.
    /// No NATS publish here — the signed envelope shape is the primary contract.
    #[tokio::test]
    async fn publish_worker_online_produces_signed_header_shape() {
        let (_tmp, ctx) = build_ctx().await;
        let snapshot = WorkerOnlineSnapshot {
            worker_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            last_checkpoint_id: None,
            last_wal_seq: 0,
            tasks_in_progress: vec![TaskProgress {
                task_id: Uuid::new_v4(),
                step: 0,
            }],
        };
        let payload = serde_json::to_vec(&snapshot).unwrap();
        let header = ctx
            .sign_outbound_worker(Uuid::new_v4(), &payload)
            .expect("sign worker-online");
        let env = SignatureEnvelope::decode_header(&header).expect("decode header");
        assert_eq!(env.fields.direction, Direction::WorkerToServer);
        assert_eq!(env.fields.payload_hash, payload_sha256_hex(&payload));
    }
}
