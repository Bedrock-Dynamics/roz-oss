//! Phase 26.7 SC2: `ArtifactService` gRPC implementation.
//!
//! # Tenant scope
//! `auth_ext::tenant_from_extensions` runs at every handler entry. DB
//! helpers in `roz_db::session_artifacts` bind `tenant_id` EXPLICITLY in
//! SQL (the superuser pool bypasses RLS). Handler-side defense-in-depth
//! `row.tenant_id != caller_tenant` checks remain as belt-and-braces.
//!
//! # Path safety (D-12)
//! `ROZ_ARTIFACT_DIR` is canonicalized at boot (see `main.rs`).
//! `DownloadArtifact` canonicalizes every stored `row.path` and verifies
//! `starts_with(artifact_dir)` — same idiom as
//! `crates/roz-server/src/grpc/observability.rs`.

#![allow(clippy::result_large_err, reason = "tonic Status is large by design")]

use std::path::PathBuf;

use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::auth_ext;
use crate::grpc::roz_v1::artifact_service_server::ArtifactService;
use crate::grpc::roz_v1::{
    ArtifactSummary, DownloadArtifactChunk, DownloadArtifactRequest, ListSessionArtifactsRequest,
    ListSessionArtifactsResponse, UploadArtifactChunk, UploadArtifactRequest, UploadArtifactResponse,
    upload_artifact_request,
};

/// 1 MiB upload/download chunk size (D-10).
pub const UPLOAD_CHUNK_SIZE: usize = 1024 * 1024;

/// Channel capacity for the download producer. 4 × 1 MiB = ~4 MiB ceiling.
const DOWNLOAD_CHANNEL_CAPACITY: usize = 4;

#[derive(Debug, Clone)]
pub struct ArtifactServiceImpl {
    pool: PgPool,
    artifact_dir: PathBuf,
}

impl ArtifactServiceImpl {
    #[must_use]
    pub const fn new(pool: PgPool, artifact_dir: PathBuf) -> Self {
        Self { pool, artifact_dir }
    }

    #[must_use]
    pub fn extension_for(artifact_type: &str) -> &'static str {
        match artifact_type {
            "copper" => "copper",
            "ulog" => "ulg",
            "video" => "mp4",
            "bundle" => "tar",
            _ => "bin",
        }
    }

    /// D-03: `'mcap'` is reserved in the DB CHECK enum but MUST NOT be
    /// written by this service this phase. MCAPs continue to live in
    /// `roz_session_mcap_archives`.
    #[must_use]
    pub fn content_type_is_allowed_this_phase(artifact_type: &str) -> bool {
        matches!(artifact_type, "copper" | "ulog" | "video" | "bundle")
    }
}

#[tonic::async_trait]
impl ArtifactService for ArtifactServiceImpl {
    async fn upload_artifact(
        &self,
        request: Request<tonic::Streaming<UploadArtifactRequest>>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let caller_tenant = auth_ext::tenant_from_extensions(&request)?;
        let mut stream = request.into_inner();

        // ---- First frame MUST be metadata.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty stream"))?;
        let metadata = match first.payload {
            Some(upload_artifact_request::Payload::Metadata(m)) => m,
            _ => return Err(Status::invalid_argument("first frame must be metadata")),
        };

        let session_id =
            Uuid::parse_str(&metadata.session_id).map_err(|_| Status::invalid_argument("invalid session_id"))?;

        if !Self::content_type_is_allowed_this_phase(&metadata.artifact_type) {
            return Err(Status::invalid_argument("artifact_type not allowed this phase (D-03)"));
        }

        if metadata.digest_sha256.len() != 32 {
            return Err(Status::invalid_argument("digest_sha256 must be 32 bytes"));
        }

        // ---- Compute target paths. artifact_id + UUID-only components
        // ensure no `..` can appear by construction (D-12).
        let artifact_id = Uuid::new_v4();
        let ext = Self::extension_for(&metadata.artifact_type);
        let dir = self
            .artifact_dir
            .join(caller_tenant.to_string())
            .join(session_id.to_string());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| Status::internal(format!("mkdir failed: {e}")))?;
        let canonical_path = dir.join(format!("{artifact_id}.{ext}"));
        let tmp_path = dir.join(format!("{artifact_id}.{ext}.tmp"));

        // ---- Stream chunks into the tmp file, hashing on the fly.
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| Status::internal(format!("create tmp failed: {e}")))?;
        let mut hasher = Sha256::new();
        let mut bytes_written: u64 = 0;

        let result: Result<(), Status> = async {
            while let Some(msg) = stream.message().await? {
                let chunk = match msg.payload {
                    Some(upload_artifact_request::Payload::Chunk(UploadArtifactChunk { data })) => data,
                    Some(upload_artifact_request::Payload::Metadata(_)) => {
                        return Err(Status::invalid_argument("metadata frame after stream start"));
                    }
                    None => continue,
                };
                if chunk.is_empty() {
                    continue;
                }
                bytes_written = bytes_written.saturating_add(chunk.len() as u64);
                if bytes_written > metadata.size_bytes {
                    return Err(Status::invalid_argument("chunk count exceeds declared size"));
                }
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(|e| Status::internal(format!("write tmp failed: {e}")))?;
            }
            file.flush()
                .await
                .map_err(|e| Status::internal(format!("flush tmp failed: {e}")))?;
            Ok(())
        }
        .await;

        // Ensure file handle is closed before any rename/remove.
        drop(file);

        if let Err(status) = result {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(status);
        }

        // ---- Verify size + digest.
        if bytes_written != metadata.size_bytes {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(Status::invalid_argument("size mismatch"));
        }
        let computed = hasher.finalize();
        if computed.as_slice() != metadata.digest_sha256.as_slice() {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(Status::invalid_argument("digest mismatch"));
        }

        // ---- Atomic rename tmp → canonical, then INSERT row.
        tokio::fs::rename(&tmp_path, &canonical_path)
            .await
            .map_err(|e| Status::internal(format!("rename tmp→canonical failed: {e}")))?;

        let path_str = canonical_path.to_string_lossy().into_owned();
        let row = roz_db::session_artifacts::insert(
            &self.pool,
            caller_tenant,
            session_id,
            &metadata.artifact_type,
            &path_str,
            &metadata.digest_sha256,
            i64::try_from(bytes_written).map_err(|_| Status::internal("size too large"))?,
            &metadata.content_type,
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, %session_id, %artifact_id, "artifact INSERT failed");
            let orphan = canonical_path.clone();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(&orphan).await;
            });
            Status::internal("artifact insert failed")
        })?;

        tracing::info!(
            artifact_id = %row.artifact_id,
            %session_id,
            tenant_id = %caller_tenant,
            artifact_type = %metadata.artifact_type,
            size_bytes = bytes_written,
            "artifact uploaded"
        );

        Ok(Response::new(UploadArtifactResponse {
            artifact_id: row.artifact_id.to_string(),
            size_bytes: bytes_written,
        }))
    }

    type DownloadArtifactStream = ReceiverStream<Result<DownloadArtifactChunk, Status>>;

    async fn download_artifact(
        &self,
        request: Request<DownloadArtifactRequest>,
    ) -> Result<Response<Self::DownloadArtifactStream>, Status> {
        let caller_tenant = auth_ext::tenant_from_extensions(&request)?;
        let artifact_id_str = request.into_inner().artifact_id;
        let artifact_id =
            Uuid::parse_str(&artifact_id_str).map_err(|_| Status::invalid_argument("invalid artifact_id"))?;

        let row = roz_db::session_artifacts::fetch_by_id(&self.pool, caller_tenant, artifact_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %artifact_id, "fetch_by_id failed");
                Status::internal("artifact lookup failed")
            })?
            .ok_or_else(|| Status::not_found("artifact not found"))?;

        if row.tenant_id != caller_tenant {
            tracing::warn!(
                %artifact_id,
                artifact_tenant = %row.tenant_id,
                caller_tenant = %caller_tenant,
                "cross-tenant access denied"
            );
            return Err(Status::permission_denied("cross-tenant access denied"));
        }

        let canonical = std::fs::canonicalize(&row.path).map_err(|e| {
            tracing::error!(error = %e, path = %row.path, "artifact path resolution failed");
            Status::internal("artifact path resolution failed")
        })?;
        if !canonical.starts_with(&self.artifact_dir) {
            tracing::error!(
                path = %row.path,
                canonical = %canonical.display(),
                artifact_dir = %self.artifact_dir.display(),
                "artifact path outside ROZ_ARTIFACT_DIR; refusing"
            );
            return Err(Status::internal("artifact path outside artifact root"));
        }

        let digest = row.digest_sha256.clone();
        let size_bytes = row.size_bytes;

        let (tx, rx) = mpsc::channel::<Result<DownloadArtifactChunk, Status>>(DOWNLOAD_CHANNEL_CAPACITY);

        tokio::spawn(async move {
            let mut file = match tokio::fs::File::open(&canonical).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(error = %e, path = %canonical.display(), "open artifact failed");
                    let _ = tx.send(Err(Status::internal("open artifact failed"))).await;
                    return;
                }
            };
            let mut remaining = size_bytes.max(0);
            let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];
            while remaining > 0 {
                let n = match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = %e, "read artifact failed");
                        let _ = tx.send(Err(Status::internal("read artifact failed"))).await;
                        return;
                    }
                };
                let n_i64 = i64::try_from(n).unwrap_or(i64::MAX);
                remaining = remaining.saturating_sub(n_i64);
                let is_last = remaining == 0;
                let chunk = DownloadArtifactChunk {
                    data: buf[..n].to_vec(),
                    digest_sha256: if is_last { Some(digest.clone()) } else { None },
                };
                if tx.send(Ok(chunk)).await.is_err() {
                    tracing::debug!("download client disconnected");
                    return;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn list_session_artifacts(
        &self,
        request: Request<ListSessionArtifactsRequest>,
    ) -> Result<Response<ListSessionArtifactsResponse>, Status> {
        let caller_tenant = auth_ext::tenant_from_extensions(&request)?;
        let session_id_str = request.into_inner().session_id;
        let session_id =
            Uuid::parse_str(&session_id_str).map_err(|_| Status::invalid_argument("invalid session_id"))?;

        let rows = roz_db::session_artifacts::list_by_session(&self.pool, caller_tenant, session_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %session_id, "list_by_session failed");
                Status::internal("artifact listing failed")
            })?;

        let mut artifacts = Vec::with_capacity(rows.len());
        for row in rows {
            if row.tenant_id != caller_tenant {
                tracing::warn!(
                    artifact_id = %row.artifact_id,
                    artifact_tenant = %row.tenant_id,
                    caller_tenant = %caller_tenant,
                    "tenant-scope leak? skipping cross-tenant row in list_session_artifacts"
                );
                continue;
            }
            artifacts.push(ArtifactSummary {
                artifact_id: row.artifact_id.to_string(),
                artifact_type: row.artifact_type,
                path: row.path,
                digest_sha256: row.digest_sha256,
                size_bytes: u64::try_from(row.size_bytes).unwrap_or(0),
                content_type: row.content_type,
            });
        }

        Ok(Response::new(ListSessionArtifactsResponse { artifacts }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_for_maps_artifact_type_to_on_disk_extension() {
        assert_eq!(ArtifactServiceImpl::extension_for("copper"), "copper");
        assert_eq!(ArtifactServiceImpl::extension_for("ulog"), "ulg");
        assert_eq!(ArtifactServiceImpl::extension_for("video"), "mp4");
        assert_eq!(ArtifactServiceImpl::extension_for("bundle"), "tar");
        assert_eq!(ArtifactServiceImpl::extension_for("weird"), "bin");
    }

    #[test]
    fn content_type_is_allowed_rejects_mcap_this_phase() {
        assert!(!ArtifactServiceImpl::content_type_is_allowed_this_phase("mcap"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("copper"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("ulog"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("video"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("bundle"));
        assert!(!ArtifactServiceImpl::content_type_is_allowed_this_phase("garbage"));
    }

    #[test]
    fn upload_chunk_size_is_one_mib() {
        assert_eq!(UPLOAD_CHUNK_SIZE, 1024 * 1024);
    }
}
