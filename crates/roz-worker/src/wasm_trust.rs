//! Worker-side wrapper around `roz_copper::wasm::CuWasmTask::from_precompiled`.
//!
//! Translates `WasmLoadError` to a `roz_nats::WasmTrustFailure` event on
//! failure and publishes it to `safety.trust_failure.{worker_id}`.
//!
//! Kept here (not in roz-copper) so the WASM loader stays transport-
//! agnostic. See Phase 14 REVIEWS.md MEDIUM.
//!
//! # Phase 23 FS-04 signing (plan 23-12)
//!
//! Trust-failure publishes are authenticity-bearing — a forged
//! `WasmTrustFailure` would misdirect the server's trust posture state
//! machine. When a [`crate::signing_hooks::WorkerSigningContext`] is
//! supplied, the publish carries a `roz-sig-v1` header bound to the
//! host UUID as correlation (host-scoped subject, matching the
//! `publish_trust_report_signed` precedent from plan 23-08). A `None`
//! context falls back to the D-12 unsigned legacy path.

use roz_copper::wasm_signature::{TrustedKeys, WasmLoadError};
use roz_nats::{Subjects, WasmTrustFailure};
use uuid::Uuid;

use crate::signing_hooks::WorkerSigningContext;

/// Load a signed `.cwasm` module via `roz-copper`.
///
/// On [`WasmLoadError`], translate the error to a
/// [`roz_nats::WasmTrustFailure`] event, publish it to
/// `safety.trust_failure.{worker_id}` (fire-and-forget — publish errors
/// are logged at `warn!` and swallowed), and return the original error.
///
/// `signing_ctx` + `host_id` are forwarded to the publish site for
/// Phase 23 FS-04 signing. When `signing_ctx` is `None`, the publish runs
/// through the unsigned legacy path (D-12 rollout window).
///
/// # Errors
/// Returns any `anyhow::Error` bubbled up from `from_precompiled`.
pub async fn load_precompiled_signed(
    nats: &async_nats::Client,
    signing_ctx: Option<&WorkerSigningContext>,
    host_id: Uuid,
    worker_id: &str,
    cwasm: &[u8],
    sig: &[u8],
    keyset: &TrustedKeys,
) -> anyhow::Result<roz_copper::wasm::CuWasmTask> {
    match roz_copper::wasm::CuWasmTask::from_precompiled(cwasm, sig, keyset) {
        Ok(task) => Ok(task),
        Err(err) => {
            if let Some(wasm_err) = err.downcast_ref::<WasmLoadError>() {
                let event = event_from_error(worker_id, wasm_err);
                publish(nats, signing_ctx, host_id, worker_id, &event).await;
            }
            Err(err)
        }
    }
}

fn event_from_error(worker_id: &str, err: &WasmLoadError) -> WasmTrustFailure {
    let occurred_at = chrono::Utc::now().to_rfc3339();
    match err {
        WasmLoadError::SignatureInvalid {
            key_id,
            module_id,
            version,
            reason,
        } => WasmTrustFailure {
            worker_id: worker_id.into(),
            key_id: key_id.clone(),
            module_id: module_id.clone(),
            version: version.clone(),
            reason: (*reason).to_string(),
            occurred_at,
        },
        WasmLoadError::UnknownKeyId(kid) => WasmTrustFailure {
            worker_id: worker_id.into(),
            key_id: kid.clone(),
            module_id: "<unknown>".into(),
            version: "<unknown>".into(),
            reason: "unknown key_id".into(),
            occurred_at,
        },
        WasmLoadError::EnvelopeDecode(msg) => WasmTrustFailure {
            worker_id: worker_id.into(),
            key_id: "<unknown>".into(),
            module_id: "<unknown>".into(),
            version: "<unknown>".into(),
            reason: format!("envelope decode failed: {msg}"),
            occurred_at,
        },
        WasmLoadError::KeysetConfig(kc) => WasmTrustFailure {
            worker_id: worker_id.into(),
            key_id: "<unknown>".into(),
            module_id: "<unknown>".into(),
            version: "<unknown>".into(),
            reason: format!("keyset config invalid: {kc}"),
            occurred_at,
        },
        WasmLoadError::IdentityMismatch(m) => WasmTrustFailure {
            worker_id: worker_id.into(),
            key_id: "<unknown>".into(),
            module_id: m.actual_module_id.clone(),
            version: m.actual_version.clone(),
            reason: format!(
                "identity mismatch: expected {}@{}, got {}@{}",
                m.expected_module_id, m.expected_version, m.actual_module_id, m.actual_version
            ),
            occurred_at,
        },
    }
}

async fn publish(
    nats: &async_nats::Client,
    signing_ctx: Option<&WorkerSigningContext>,
    host_id: Uuid,
    worker_id: &str,
    event: &WasmTrustFailure,
) {
    let subject = match Subjects::wasm_trust_failure(worker_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, worker_id, "build trust-failure subject failed");
            return;
        }
    };
    let payload = match serde_json::to_vec(event) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, worker_id, "serialize WasmTrustFailure failed");
            return;
        }
    };

    if let Some(ctx) = signing_ctx {
        // Phase 23 FS-04 signed path. Matches `publish_trust_report_signed`
        // shape from plan 23-08: host-scoped subject uses host UUID as
        // correlation_id so the server verifier partitions replay state
        // on (direction, host_id, sequence).
        match ctx.sign_outbound_worker(host_id, &payload) {
            Ok(header) => {
                if let Err(e) = roz_nats::dispatch::publish_signed(nats, subject, payload, &header).await {
                    tracing::warn!(error = %e, worker_id, "publish trust-failure event failed");
                }
            }
            Err(e) => {
                // Fail-closed on sign error (matches trust.rs + dispatch.rs
                // behavior in plan 23-08 — better to drop the notification
                // than forge an unsigned envelope under signed enforcement).
                tracing::warn!(error = %e, worker_id, "sign trust-failure event failed; dropping");
            }
        }
    } else if let Err(e) = nats.publish(subject, payload.into()).await {
        // D-12 rollout-window fallback: no signing context available.
        tracing::warn!(error = %e, worker_id, "publish trust-failure event failed (unsigned path)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_copper::wasm_signature::IdentityMismatch;

    #[test]
    fn event_from_signature_invalid_preserves_all_fields() {
        let err = WasmLoadError::SignatureInvalid {
            key_id: "k1".into(),
            module_id: "m1".into(),
            version: "1".into(),
            reason: "ed25519 verify failed",
        };
        let evt = event_from_error("w1", &err);
        assert_eq!(evt.worker_id, "w1");
        assert_eq!(evt.key_id, "k1");
        assert_eq!(evt.module_id, "m1");
        assert_eq!(evt.version, "1");
        assert_eq!(evt.reason, "ed25519 verify failed");
        assert!(!evt.occurred_at.is_empty());
    }

    #[test]
    fn event_from_unknown_key_id_fills_unknowns() {
        let err = WasmLoadError::UnknownKeyId("rotated".into());
        let evt = event_from_error("w1", &err);
        assert_eq!(evt.key_id, "rotated");
        assert_eq!(evt.module_id, "<unknown>");
        assert_eq!(evt.reason, "unknown key_id");
    }

    #[test]
    fn event_from_envelope_decode_prefixes_reason() {
        let err = WasmLoadError::EnvelopeDecode("trailing bytes".into());
        let evt = event_from_error("w1", &err);
        assert!(evt.reason.starts_with("envelope decode failed:"));
        assert!(evt.reason.contains("trailing bytes"));
    }

    #[test]
    fn event_from_identity_mismatch_preserves_actuals() {
        let err = WasmLoadError::IdentityMismatch(IdentityMismatch {
            expected_module_id: "arm".into(),
            expected_version: "1.2.0".into(),
            actual_module_id: "leg".into(),
            actual_version: "0.9.0".into(),
        });
        let evt = event_from_error("w1", &err);
        assert_eq!(evt.module_id, "leg");
        assert_eq!(evt.version, "0.9.0");
        assert!(evt.reason.contains("arm@1.2.0"));
        assert!(evt.reason.contains("leg@0.9.0"));
    }
}
