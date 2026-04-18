//! Signed server→worker re-arm subscriber (FS-01 D-02).
//!
//! Subscribes to `cmd.{worker_id}.clear_failsafe`, verifies each inbound
//! message via [`WorkerSigningContext::verify_inbound_worker`], and on
//! success calls [`CommandWatchdog::clear_failsafe`] to un-latch motion.
//!
//! The message body carries no per-call parameters today — the signed
//! envelope's `correlation_id` plus (optional) operator-provided reason
//! string is sufficient for audit. Future needs may extend
//! [`ClearFailsafePayload`].
//!
//! # Threat notes
//!
//! - **T-24-51** (unsigned re-arm bypass): verification runs BEFORE latch
//!   mutation; missing header → [`ClearFailsafeError::Signing`] with
//!   [`WorkerSigningError::MissingHeader`].
//! - **T-24-52** (forged re-arm): wrong server signing key → verify
//!   failure; latch unchanged.
//! - **T-24-50** (replay): Phase 23's replay gate (server-side
//!   `last_acked_seq`) protects the outbound signing path; any replayed
//!   envelope still verifies cryptographically but cannot be produced by
//!   the server twice. This module surfaces signature failures verbatim.

use std::sync::Arc;

use async_nats::Message;
use roz_core::signing::HEADER_NAME;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command_watchdog::CommandWatchdog;
use crate::signing_hooks::{WorkerSigningContext, WorkerSigningError};

/// Operator metadata carried inside a signed `clear_failsafe` envelope.
/// All fields are optional — an empty body is valid.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ClearFailsafePayload {
    /// Free-text operator-provided reason recorded in the audit log.
    #[serde(default)]
    pub reason: Option<String>,
    /// Operator identifier (e.g. API-key id, user subject) attached by the
    /// server on behalf of the caller.
    #[serde(default)]
    pub operator: Option<String>,
}

/// Failure modes surfaced by the clear-failsafe message handler.
#[derive(Debug, Error)]
pub enum ClearFailsafeError {
    /// Inbound signature verification rejected the message.
    #[error("signature verification: {0}")]
    Signing(#[from] WorkerSigningError),
    /// Payload bytes were not valid JSON (after signature verification
    /// already bound the bytes to the signer).
    #[error("deserialize payload: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// Handle a single inbound `cmd.{worker_id}.clear_failsafe` message.
///
/// Verifies the `roz-sig-v1` envelope FIRST (fail-closed on unsigned,
/// forged, or wrong-direction messages). On success, calls
/// [`CommandWatchdog::clear_failsafe`] to un-latch motion and returns the
/// decoded payload for audit.
///
/// Idempotent on an already-cleared latch — re-verified messages simply
/// re-confirm the cleared state.
///
/// # Errors
///
/// Any error aborts the clear action; motion remains latched.
#[tracing::instrument(
    level = "info",
    skip(signing_ctx, watchdog, message),
    fields(subject = %message.subject)
)]
pub fn handle_clear_failsafe_message(
    signing_ctx: &WorkerSigningContext,
    watchdog: &CommandWatchdog,
    message: &Message,
) -> Result<ClearFailsafePayload, ClearFailsafeError> {
    // Extract the roz-sig-v1 header (missing → MissingHeader).
    let header = message
        .headers
        .as_ref()
        .and_then(|h| h.get(HEADER_NAME).map(|v| v.to_string()));

    // Verify (covers direction, payload_hash, server verifying key, envelope
    // canonicalisation). MUST run before any state mutation.
    signing_ctx.verify_inbound_worker(header.as_deref(), &message.payload)?;

    // Empty body is valid (operator did not supply metadata).
    let payload: ClearFailsafePayload = if message.payload.is_empty() {
        ClearFailsafePayload::default()
    } else {
        serde_json::from_slice(&message.payload)?
    };

    watchdog.clear_failsafe();
    tracing::info!(
        operator = payload.operator.as_deref().unwrap_or("-"),
        reason = payload.reason.as_deref().unwrap_or("-"),
        "failsafe latch cleared"
    );
    Ok(payload)
}

/// Top-level subscriber task. Spawn under worker main once NATS is connected.
///
/// Main.rs wiring lands in Plan 24-09 — this plan ships the handler + loop
/// so integration tests and the main runtime have a stable surface.
///
/// # Errors
///
/// Returns on subscribe-setup failure or cancel. Per-message errors are
/// logged via `tracing::warn!` and the loop continues.
pub async fn run_clear_failsafe_subscriber(
    nats: async_nats::Client,
    worker_id: String,
    signing_ctx: Arc<WorkerSigningContext>,
    watchdog: Arc<CommandWatchdog>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let subject =
        roz_nats::Subjects::clear_failsafe(&worker_id).map_err(|e| anyhow::anyhow!("invalid worker_id: {e}"))?;
    let mut sub = nats.subscribe(subject.clone()).await?;
    tracing::info!(subject = %subject, "clear_failsafe subscriber ready");

    loop {
        tokio::select! {
            maybe_msg = futures::StreamExt::next(&mut sub) => {
                match maybe_msg {
                    Some(msg) => {
                        if let Err(e) = handle_clear_failsafe_message(&signing_ctx, &watchdog, &msg) {
                            tracing::warn!(error = %e, "clear_failsafe message rejected");
                        }
                    }
                    None => {
                        tracing::warn!("clear_failsafe subscription ended");
                        return Ok(());
                    }
                }
            }
            () = cancel.cancelled() => {
                tracing::debug!("clear_failsafe subscriber cancelled");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use crate::wal::WalStore;
    use async_nats::HeaderMap;
    use chrono::Utc;
    use ed25519_dalek::SigningKey;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use roz_core::signing::{Direction, SignedFields, payload_sha256_hex, sign_envelope};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

    /// Build a worker signing context with a known server signing key cached
    /// on disk. Returns (tempdir, ctx, server signing key, tenant, host).
    async fn ctx_with_server_key() -> (TempDir, Arc<WorkerSigningContext>, SigningKey, Uuid, Uuid) {
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
        let wal_path = tmp.path().join("wal.db");
        let wal = Arc::new(WalStore::open(wal_path.to_str().unwrap()).unwrap());
        let ctx = Arc::new(WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal));
        (tmp, ctx, server_signing, tenant, host)
    }

    fn build_signed_message(
        subject: &str,
        payload: Vec<u8>,
        server_signing: &SigningKey,
        tenant: Uuid,
        host: Uuid,
    ) -> Message {
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: tenant,
            host_id: host,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(&payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, server_signing).unwrap();
        let header_value = env.encode_header().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_NAME, header_value.as_str());
        Message {
            subject: subject.to_string().into(),
            reply: None,
            payload: payload.into(),
            headers: Some(headers),
            status: None,
            description: None,
            length: 0,
        }
    }

    #[tokio::test]
    async fn valid_signed_clear_failsafe_clears_latch() {
        let (_tmp, ctx, server, tenant, host) = ctx_with_server_key().await;
        let wd = Arc::new(CommandWatchdog::new(Duration::from_secs(3600)));
        wd.latched.store(true, Ordering::Relaxed);
        assert!(wd.is_latched());

        let payload = serde_json::to_vec(&ClearFailsafePayload {
            reason: Some("manual re-arm".into()),
            operator: Some("op-1".into()),
        })
        .unwrap();
        let msg = build_signed_message("cmd.host1.clear_failsafe", payload, &server, tenant, host);

        let decoded = handle_clear_failsafe_message(&ctx, &wd, &msg).unwrap();
        assert!(!wd.is_latched(), "latch must clear on valid signed message");
        assert_eq!(decoded.operator.as_deref(), Some("op-1"));
        assert_eq!(decoded.reason.as_deref(), Some("manual re-arm"));
    }

    #[tokio::test]
    async fn unsigned_clear_failsafe_rejected() {
        let (_tmp, ctx, _server, _tenant, _host) = ctx_with_server_key().await;
        let wd = Arc::new(CommandWatchdog::new(Duration::from_secs(3600)));
        wd.latched.store(true, Ordering::Relaxed);

        let msg = Message {
            subject: "cmd.host1.clear_failsafe".to_string().into(),
            reply: None,
            payload: b"{}".to_vec().into(),
            headers: None,
            status: None,
            description: None,
            length: 0,
        };
        let err = handle_clear_failsafe_message(&ctx, &wd, &msg).unwrap_err();
        assert!(matches!(err, ClearFailsafeError::Signing(_)));
        assert!(wd.is_latched(), "latch must NOT clear on unsigned message");
    }

    #[tokio::test]
    async fn forged_signature_rejected() {
        let (_tmp, ctx, _server, tenant, host) = ctx_with_server_key().await;
        let wd = Arc::new(CommandWatchdog::new(Duration::from_secs(3600)));
        wd.latched.store(true, Ordering::Relaxed);

        let attacker = SigningKey::from_bytes(&[42u8; 32]);
        let payload = b"{}".to_vec();
        let msg = build_signed_message("cmd.host1.clear_failsafe", payload, &attacker, tenant, host);

        let err = handle_clear_failsafe_message(&ctx, &wd, &msg).unwrap_err();
        assert!(matches!(err, ClearFailsafeError::Signing(_)));
        assert!(wd.is_latched(), "latch must NOT clear on forged signature");
    }

    #[tokio::test]
    async fn wrong_direction_rejected() {
        let (_tmp, ctx, server, tenant, host) = ctx_with_server_key().await;
        let wd = Arc::new(CommandWatchdog::new(Duration::from_secs(3600)));
        wd.latched.store(true, Ordering::Relaxed);

        let payload = b"{}".to_vec();
        let fields = SignedFields {
            direction: Direction::WorkerToServer, // wrong direction for inbound
            tenant_id: tenant,
            host_id: host,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(&payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server).unwrap();
        let header_value = env.encode_header().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_NAME, header_value.as_str());
        let msg = Message {
            subject: "cmd.host1.clear_failsafe".to_string().into(),
            reply: None,
            payload: payload.into(),
            headers: Some(headers),
            status: None,
            description: None,
            length: 0,
        };
        let err = handle_clear_failsafe_message(&ctx, &wd, &msg).unwrap_err();
        assert!(matches!(err, ClearFailsafeError::Signing(_)));
        assert!(wd.is_latched(), "latch must NOT clear on wrong direction");
    }

    #[tokio::test]
    async fn idempotent_on_already_cleared() {
        let (_tmp, ctx, server, tenant, host) = ctx_with_server_key().await;
        let wd = Arc::new(CommandWatchdog::new(Duration::from_secs(3600)));
        assert!(!wd.is_latched(), "precondition: un-latched");

        let payload = b"{}".to_vec();
        let msg = build_signed_message("cmd.host1.clear_failsafe", payload, &server, tenant, host);
        // Idempotent — no error on already-cleared state.
        handle_clear_failsafe_message(&ctx, &wd, &msg).unwrap();
        assert!(!wd.is_latched());
    }
}
