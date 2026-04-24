//! Phase 26.7 SC4 + SC6: session-end copper log archival.
//!
//! # Contract (D-16)
//! `finalize_copper_archive` NEVER panics and NEVER returns `Err`. All
//! failure modes log via `tracing::warn!` with structured fields and
//! return `Ok(())`.
//!
//! # Drop-ordering invariant (Q1)
//! cu29-unifiedlog 0.14 has no public flush/close method; segments only
//! sync to disk on `Arc<Mutex<UnifiedLoggerWrite>>` drop. The caller in
//! `session_relay.rs` MUST drop every handle held for this `session_id`
//! before awaiting `finalize_copper_archive`. Violating this produces
//! a truncated upload with an otherwise-valid digest.
//!
//! # cu29 filename format
//! cu29-unifiedlog 0.14 emits segments as `{stem}_{N}.{ext}` — e.g.
//! `session_0.copper`. This module enumerates via
//! `read_dir + extension == "copper"` which is tolerant of both the
//! bare alias `session.copper` (if present) and numbered segments.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;
use tokio_stream::wrappers::ReceiverStream;

use crate::observability_config::ObservabilityCopperConfig;
use crate::roz_v1::artifact_service_client::ArtifactServiceClient;
use crate::roz_v1::{UploadArtifactChunk, UploadArtifactMetadata, UploadArtifactRequest, upload_artifact_request};

/// 1 MiB chunk size; must match the server-side constant in
/// `roz_server::grpc::artifacts::UPLOAD_CHUNK_SIZE`.
pub const UPLOAD_CHUNK_SIZE_WORKER: usize = 1024 * 1024;

/// Default MIME for copper unified logs (D-19). Informational only.
pub const COPPER_CONTENT_TYPE: &str = "application/vnd.copper-log";

/// Stream-upload every `*.copper` file under `{data_dir}/sessions/{session_id}/`
/// to the server via `ArtifactService.UploadArtifact`. Soft-fails per D-16.
///
/// On success + `!config.keep_local_after_upload`, removes the session dir.
/// On any per-file failure, retains the session dir (next retry re-enumerates).
pub async fn finalize_copper_archive(
    data_dir: &Path,
    session_id: &str,
    config: &ObservabilityCopperConfig,
    client: ArtifactServiceClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let session_dir = data_dir.join("sessions").join(session_id);

    let segments = match enumerate_copper_segments(&session_dir).await {
        Ok(v) if v.is_empty() => {
            tracing::debug!(
                session_id,
                session_dir = %session_dir.display(),
                "no .copper segments to archive"
            );
            return Ok(());
        }
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                session_id,
                session_dir = %session_dir.display(),
                error = %e,
                "failed to enumerate copper segments; skipping archival"
            );
            return Ok(());
        }
    };

    let mut all_succeeded = true;
    for segment in &segments {
        match upload_single_segment(segment, session_id, client.clone()).await {
            Ok(()) => {
                tracing::info!(
                    session_id,
                    path = %segment.display(),
                    "copper segment uploaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id,
                    path = %segment.display(),
                    error = %e,
                    "copper segment upload failed (soft-fail)"
                );
                all_succeeded = false;
            }
        }
    }

    if all_succeeded && !config.keep_local_after_upload {
        match tokio::fs::remove_dir_all(&session_dir).await {
            Ok(()) => tracing::info!(
                session_id,
                session_dir = %session_dir.display(),
                "removed local session dir after successful upload"
            ),
            Err(e) => tracing::warn!(
                session_id,
                session_dir = %session_dir.display(),
                error = %e,
                "failed to remove local session dir after upload"
            ),
        }
    }

    Ok(())
}

async fn enumerate_copper_segments(session_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(session_dir).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let is_copper = path.extension().and_then(|x| x.to_str()).is_some_and(|e| e == "copper");
        if is_copper && entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

async fn upload_single_segment(
    segment: &Path,
    session_id: &str,
    mut client: ArtifactServiceClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let digest = sha256_of_file(segment).await?;
    let metadata = tokio::fs::metadata(segment).await?;
    let size_bytes = metadata.len();
    let path_in_row = segment
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("segment path has no file_name"))?
        .to_string_lossy()
        .into_owned();

    let file = tokio::fs::File::open(segment).await?;

    let (tx, rx) = tokio::sync::mpsc::channel::<UploadArtifactRequest>(4);

    let producer_session_id = session_id.to_string();
    let producer_digest = digest.to_vec();
    let producer_path = path_in_row.clone();
    tokio::spawn(async move {
        let metadata_frame = UploadArtifactRequest {
            payload: Some(upload_artifact_request::Payload::Metadata(UploadArtifactMetadata {
                session_id: producer_session_id,
                artifact_type: "copper".to_string(),
                path: producer_path,
                size_bytes,
                digest_sha256: producer_digest,
                content_type: COPPER_CONTENT_TYPE.to_string(),
            })),
        };
        if tx.send(metadata_frame).await.is_err() {
            return;
        }
        let mut file = file;
        let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE_WORKER];
        loop {
            let n = match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "read copper segment chunk failed");
                    return;
                }
            };
            let chunk_frame = UploadArtifactRequest {
                payload: Some(upload_artifact_request::Payload::Chunk(UploadArtifactChunk {
                    data: buf[..n].to_vec(),
                })),
            };
            if tx.send(chunk_frame).await.is_err() {
                return;
            }
        }
    });

    let response = client
        .upload_artifact(tonic::Request::new(ReceiverStream::new(rx)))
        .await?
        .into_inner();

    if response.size_bytes != size_bytes {
        return Err(anyhow::anyhow!(
            "server-observed size {} != declared size {size_bytes}",
            response.size_bytes
        ));
    }

    Ok(())
}

async fn sha256_of_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::{enumerate_copper_segments, sha256_of_file};
    use tempfile::TempDir;

    #[tokio::test]
    async fn sha256_of_file_known_bytes() {
        let tmp = TempDir::new().expect("tmp");
        let path = tmp.path().join("x");
        tokio::fs::write(&path, b"hello").await.expect("write");
        let d = sha256_of_file(&path).await.expect("hash");
        let hex = d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[tokio::test]
    async fn enumerate_returns_empty_when_missing() {
        let tmp = TempDir::new().expect("tmp");
        let nonexistent = tmp.path().join("never-created");
        let v = enumerate_copper_segments(&nonexistent).await.expect("ok");
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn enumerate_finds_underscore_and_base_files_sorted() {
        let tmp = TempDir::new().expect("tmp");
        for name in [
            "session_2.copper",
            "session_0.copper",
            "session_1.copper",
            "session.copper",
            "other.txt",
        ] {
            tokio::fs::write(tmp.path().join(name), b"x").await.expect("write");
        }
        let v = enumerate_copper_segments(tmp.path()).await.expect("ok");
        let names: Vec<String> = v
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "session.copper".to_string(),
                "session_0.copper".to_string(),
                "session_1.copper".to_string(),
                "session_2.copper".to_string(),
            ]
        );
    }
}
