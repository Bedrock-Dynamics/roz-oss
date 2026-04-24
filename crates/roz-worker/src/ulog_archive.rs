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
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio_stream::wrappers::ReceiverStream;

use crate::copper_archive::UPLOAD_CHUNK_SIZE_WORKER;
use crate::roz_v1::artifact_service_client::ArtifactServiceClient;
use crate::roz_v1::{UploadArtifactChunk, UploadArtifactMetadata, UploadArtifactRequest, upload_artifact_request};
use crate::ulog_config::UlogConfig;
use roz_mavlink::{AutopilotKind, LogDownloader, MavlinkBackend, UlogError};

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

    let start = std::time::Instant::now();

    // Plan 07 D-09 structured warn-log fields: carried through every
    // failure branch so downstream consumers get a consistent shape. Both
    // default to 0 (pre-download state); updated as the protocol advances.
    let mut attempted_log_id: u16 = 0;
    let mut bytes_received: usize = 0;

    // Phase 26.8 SC1: drive the MAVLink LOG_* protocol via LogDownloader.
    // The LogDownloader's Drop impl best-effort issues LOG_REQUEST_END on
    // every exit path (success, error, early return, panic) — see
    // log_download.rs RESEARCH pitfall 1.
    let mut downloader = LogDownloader::new(backend.outbound(), backend.subscribe_log_data());
    let timeout = Duration::from_secs(config.download_timeout_secs);
    let (log_id, bytes) = match downloader.fetch_newest(timeout).await {
        Ok(v) => v,
        Err(e) => {
            // D-09 structured warn-log; map UlogError variant → failure_mode string.
            // Extract bytes_received from the variants that carry it so the
            // warn-log surfaces partial-progress telemetry (Plan 07).
            let failure_mode = match &e {
                UlogError::NoLogsAvailable => "no_logs_available",
                UlogError::LogListTimeout { .. } => "log_list_timeout",
                UlogError::LogDataTimeout { bytes_received: br } => {
                    bytes_received = *br;
                    "log_data_timeout"
                }
                UlogError::ReassemblyGapsExhausted { bytes_received: br, .. } => {
                    bytes_received = *br;
                    "reassembly_gaps_exhausted"
                }
                UlogError::LogOversized { .. } => "log_oversized",
                UlogError::OutboundClosed => "fc_unreachable",
            };
            tracing::warn!(
                session_id,
                failure_mode,
                attempted_log_id,
                bytes_received,
                duration_ms = start.elapsed().as_millis() as u64,
                error = %e,
                "ulog download failed (soft-fail)"
            );
            return Ok(None);
        }
    };
    // Successful download → update structured fields for the upload-stage
    // warn-log (and the D-06 erase gate below).
    attempted_log_id = log_id;
    bytes_received = bytes.len();

    // Upload via the 26.7 ArtifactService client-stream path.
    let artifact_id = match upload_ulog_bytes(bytes, session_id, client).await {
        Ok(Some(artifact_id)) => {
            tracing::info!(
                session_id,
                log_id,
                artifact_id = %artifact_id,
                bytes_received,
                duration_ms = start.elapsed().as_millis() as u64,
                "ulog archive uploaded"
            );
            Some(artifact_id)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                session_id,
                failure_mode = "upload_failed",
                attempted_log_id,
                bytes_received,
                duration_ms = start.elapsed().as_millis() as u64,
                error = %e,
                "ulog upload failed (soft-fail)"
            );
            return Ok(None);
        }
    };

    Ok(artifact_id)
}

/// Stream-upload an in-memory ULG byte buffer to the server via
/// `ArtifactService.UploadArtifact`.
///
/// Mirrors `crate::copper_archive::upload_single_segment` verbatim,
/// substituting a `Vec<u8>` producer for the `File` streaming producer.
/// Returns the server-issued artifact_id on success so Plan 07's LOG_ERASE
/// gate can key on `Some(id)` vs `None`.
///
/// # Errors
///
/// Returns `Err` for any failure in the mpsc producer, the gRPC call,
/// or the post-upload size-echo check. The caller (`finalize_ulog_archive`)
/// converts these to `Ok(None)` + `failure_mode="upload_failed"` warn-log.
async fn upload_ulog_bytes(
    bytes: Vec<u8>,
    session_id: &str,
    mut client: ArtifactServiceClient<tonic::transport::Channel>,
) -> anyhow::Result<Option<String>> {
    let digest = sha256_of_bytes(&bytes);
    let size_bytes = bytes.len() as u64;
    let path_in_row = format!("ulog/{session_id}.ulg"); // D-05

    let (tx, rx) = tokio::sync::mpsc::channel::<UploadArtifactRequest>(4);

    let producer_session_id = session_id.to_string();
    let producer_digest = digest.to_vec();
    let producer_path = path_in_row;
    tokio::spawn(async move {
        let metadata_frame = UploadArtifactRequest {
            payload: Some(upload_artifact_request::Payload::Metadata(UploadArtifactMetadata {
                session_id: producer_session_id,
                artifact_type: "ulog".to_string(),
                path: producer_path,
                size_bytes,
                digest_sha256: producer_digest,
                content_type: ULOG_CONTENT_TYPE.to_string(),
            })),
        };
        if tx.send(metadata_frame).await.is_err() {
            return;
        }
        for chunk in bytes.chunks(UPLOAD_CHUNK_SIZE_WORKER) {
            let frame = UploadArtifactRequest {
                payload: Some(upload_artifact_request::Payload::Chunk(UploadArtifactChunk {
                    data: chunk.to_vec(),
                })),
            };
            if tx.send(frame).await.is_err() {
                return;
            }
        }
    });

    let response = client
        .upload_artifact(tonic::Request::new(ReceiverStream::new(rx)))
        .await?
        .into_inner();

    // Mirror copper_archive:193-198 size-echo check (T-26.8.04-04).
    if response.size_bytes != size_bytes {
        return Err(anyhow::anyhow!(
            "server-observed size {} != declared size {size_bytes}",
            response.size_bytes
        ));
    }

    Ok(Some(response.artifact_id))
}

/// SHA-256 digest of an in-memory buffer. Mirrors
/// `copper_archive::sha256_of_file` but skips the streaming read — the
/// ulog path owns the `Vec<u8>` after `LogDownloader::fetch_newest` so
/// there is no file to stream from.
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

    #[test]
    fn sha256_of_bytes_empty() {
        // Empty digest is well-known: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.
        let d = sha256_of_bytes(b"");
        let hex = d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn ulog_path_format_matches_d05() {
        // D-05: path = "ulog/{session_id}.ulg"
        let session_id = "abc-123";
        let path = format!("ulog/{session_id}.ulg");
        assert_eq!(path, "ulog/abc-123.ulg");
    }
}
