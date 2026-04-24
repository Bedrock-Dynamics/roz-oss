//! Phase 26.8 SC3: session-end ulog archival via MAVLink.
//!
//! # Contract (D-09, inherits 26.7 D-16)
//! `finalize_ulog_archive` NEVER panics and NEVER returns `Err`. All
//! failure modes log via `tracing::warn!(failure_mode = ..., ...)` with
//! structured fields and return `Ok(None)`. The single `Err` surface is
//! the outer `anyhow::Result<Option<String>>` return type; `Ok(Some(id))`
//! indicates a server-acknowledged upload, `Ok(None)` indicates any soft-
//! fail or intentional skip. Callers (Plan 06 session_relay spawn, Plan 07
//! LOG_ERASE gate) MUST treat `Ok(None)` as "do not proceed with erase".
//!
//! # Gating invariant (D-08, D-11)
//! Early-returns `Ok(None)` silently when:
//!   * `config.enabled == false`         — operator opt-out (D-08)
//!   * `backend.autopilot_kind() != Px4` — ArduPilot/.BIN deferred (D-11)
//!
//! The `mavlink_backend.is_none()` gate lives one level up at the Plan 06
//! session_relay call-site; this function's input contract assumes a live
//! backend (non-Option `Arc<MavlinkBackend>`).
//!
//! # LOG_REQUEST_END invariant (RESEARCH pitfall 1)
//! The [`LogDownloader`] carries a `Drop` impl that best-effort issues
//! `LOG_REQUEST_END` on every exit path. This module MUST NOT short-
//! circuit that drop (e.g. via `std::mem::forget`).
//!
//! # Chunk-size reuse (PATTERNS no-redefine rule)
//! This module reuses [`crate::copper_archive::UPLOAD_CHUNK_SIZE_WORKER`]
//! rather than defining its own constant — chunk size is a server-invariant
//! contract shared across all artifact types.

use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use crate::roz_v1::artifact_service_client::ArtifactServiceClient;
use crate::ulog_config::UlogConfig;
use roz_mavlink::{AutopilotKind, MavlinkBackend};

/// D-05 canonical MIME for PX4 ULog logs. Matches PX4 Flight Review's
/// upload accept list and the server-side artifact_type="ulog" mapping.
pub const ULOG_CONTENT_TYPE: &str = "application/vnd.px4.ulg";

/// Session-end finalize hook: drives the MAVLink LOG_* protocol, uploads
/// the newest ULG bytes via the 26.7 ArtifactService, and returns the
/// server-issued artifact_id on success.
///
/// # Contract
///
/// - NEVER panics.
/// - Returns `Ok(None)` for every soft-fail path (D-09): config opt-out,
///   non-PX4 autopilot, download failure, upload failure.
/// - Returns `Ok(Some(artifact_id))` only after the server acknowledges
///   the upload with a matching `size_bytes`.
/// - The outer `Err` is reserved for plumbing-level failures that
///   should propagate to the caller's tracing/metrics; all known paths
///   convert internally to `Ok(None)` + warn-log.
///
/// # Gating
///
/// Early-returns `Ok(None)` silently when `config.enabled == false`
/// (debug-log). Returns `Ok(None)` with a `failure_mode="autopilot_not_px4"`
/// warn-log when the FC reports ArduPilot or Unknown.
///
/// # Arguments
///
/// * `backend` - Arc-shared handle to the worker-boot-scoped MAVLink
///   backend. Non-Option: the Option gate lives at the Plan 06 call-site.
/// * `session_id` - Session identifier; used for both the artifact path
///   (`ulog/{session_id}.ulg`) and every structured log field.
/// * `config` - Ulog-subsystem config (Plan 01).
/// * `client` - Shared ArtifactService client; cheap to clone.
///
/// # Errors
///
/// The outer `anyhow::Result` is reserved for future plumbing failures;
/// in Phase 26.8 scope, the function never returns `Err` — every failure
/// mode soft-fails to `Ok(None)` with a structured `tracing::warn!`.
pub async fn finalize_ulog_archive(
    backend: Arc<MavlinkBackend>,
    session_id: &str,
    config: &UlogConfig,
    client: ArtifactServiceClient<tonic::transport::Channel>,
) -> anyhow::Result<Option<String>> {
    // D-08: opt-out honored silently.
    if !config.enabled {
        tracing::debug!(session_id, "ulog archival disabled by config");
        return Ok(None);
    }
    // D-11: PX4-only scope — warn+skip for ArduPilot/Unknown.
    if backend.autopilot_kind() != AutopilotKind::Px4 {
        tracing::warn!(
            session_id,
            failure_mode = "autopilot_not_px4",
            autopilot_kind = ?backend.autopilot_kind(),
            "skipping ulog archival — ArduPilot/.BIN scope deferred (D-11)"
        );
        return Ok(None);
    }

    // TODO(Task 2): drive LogDownloader + upload_ulog_bytes.
    let _ = client; // silence unused warning until Task 2 wiring lands.
    Ok(None)
}

/// SHA-256 digest of an in-memory buffer. Mirrors
/// `copper_archive::sha256_of_file` but skips the streaming read — the
/// ulog path owns the `Vec<u8>` after `LogDownloader::fetch_newest` so
/// there is no file to stream from.
#[cfg_attr(not(test), allow(dead_code))] // used by upload_ulog_bytes in Task 2; only by tests now
fn sha256_of_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::{ULOG_CONTENT_TYPE, sha256_of_bytes};

    #[test]
    fn sha256_of_bytes_known_vector() {
        // Same fixture as copper_archive::tests::sha256_of_file_known_bytes
        // — validates the helper against the canonical "hello" digest.
        let d = sha256_of_bytes(b"hello");
        let hex = d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[test]
    fn ulog_content_type_matches_d05() {
        assert_eq!(ULOG_CONTENT_TYPE, "application/vnd.px4.ulg");
    }
}
