//! Phase 26.11 Plan 05: copper archive finalizer vertical coverage.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use roz_core::auth::{AuthIdentity, Role, TenantId};
use roz_db::{create_pool, run_migrations};
use roz_worker::copper_archive::finalize_copper_archive;
use roz_worker::observability_config::ObservabilityCopperConfig;
use roz_worker::roz_v1::artifact_service_client::ArtifactServiceClient;
use roz_worker::roz_v1::artifact_service_server::{ArtifactService, ArtifactServiceServer};
use roz_worker::roz_v1::{
    DownloadArtifactChunk, DownloadArtifactRequest, ListSessionArtifactsRequest, ListSessionArtifactsResponse,
    UploadArtifactMetadata, UploadArtifactRequest, UploadArtifactResponse, upload_artifact_request,
};
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

struct DbHarness {
    pool: PgPool,
    _pg: roz_test::PgGuard,
    tenant_id: Uuid,
}

#[derive(Clone)]
struct LoopbackArtifactService {
    pool: Option<PgPool>,
    tenant_id: Uuid,
    fail_path: Option<String>,
}

impl LoopbackArtifactService {
    fn success(pool: PgPool, tenant_id: Uuid) -> Self {
        Self {
            pool: Some(pool),
            tenant_id,
            fail_path: None,
        }
    }

    fn soft_fail(fail_path: &str) -> Self {
        Self {
            pool: None,
            tenant_id: Uuid::new_v4(),
            fail_path: Some(fail_path.to_string()),
        }
    }
}

#[tonic::async_trait]
impl ArtifactService for LoopbackArtifactService {
    async fn upload_artifact(
        &self,
        request: Request<Streaming<UploadArtifactRequest>>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("stream error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("empty upload stream"))?;
        let metadata = match first.payload {
            Some(upload_artifact_request::Payload::Metadata(metadata)) => metadata,
            _ => return Err(Status::invalid_argument("first frame must be metadata")),
        };

        if self.fail_path.as_deref() == Some(metadata.path.as_str()) {
            return Err(Status::internal("simulated upload failure"));
        }

        let mut bytes = Vec::new();
        while let Some(frame) = stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("stream error: {e}")))?
        {
            match frame.payload {
                Some(upload_artifact_request::Payload::Chunk(chunk)) => bytes.extend_from_slice(&chunk.data),
                Some(upload_artifact_request::Payload::Metadata(_)) => {
                    return Err(Status::invalid_argument("metadata frame after stream start"));
                }
                None => {}
            }
        }

        verify_upload(&metadata, &bytes)?;

        let artifact_id = if let Some(pool) = self.pool.as_ref() {
            let session_id =
                Uuid::parse_str(&metadata.session_id).map_err(|_| Status::invalid_argument("invalid session_id"))?;
            let row = roz_db::session_artifacts::insert(
                pool,
                self.tenant_id,
                session_id,
                &metadata.artifact_type,
                &metadata.path,
                &metadata.digest_sha256,
                i64::try_from(bytes.len()).map_err(|_| Status::internal("size too large"))?,
                &metadata.content_type,
            )
            .await
            .map_err(|e| Status::internal(format!("artifact insert failed: {e}")))?;
            row.artifact_id.to_string()
        } else {
            Uuid::new_v4().to_string()
        };

        Ok(Response::new(UploadArtifactResponse {
            artifact_id,
            size_bytes: bytes.len() as u64,
        }))
    }

    type DownloadArtifactStream = ReceiverStream<Result<DownloadArtifactChunk, Status>>;

    async fn download_artifact(
        &self,
        _request: Request<DownloadArtifactRequest>,
    ) -> Result<Response<Self::DownloadArtifactStream>, Status> {
        Err(Status::unimplemented("not used by copper finalizer tests"))
    }

    async fn list_session_artifacts(
        &self,
        _request: Request<ListSessionArtifactsRequest>,
    ) -> Result<Response<ListSessionArtifactsResponse>, Status> {
        Err(Status::unimplemented("not used by copper finalizer tests"))
    }
}

#[tokio::test]
#[ignore = "requires ArtifactService/Postgres loopback; routed by ci-integration nextest"]
async fn uploads_one_artifact_row_per_copper_segment() {
    let harness = setup_db_harness().await;
    let tmp = TempDir::new().expect("tempdir");
    let session_id = Uuid::new_v4();
    let expected = write_copper_segments(tmp.path(), session_id).await;
    let client = spawn_client(LoopbackArtifactService::success(
        harness.pool.clone(),
        harness.tenant_id,
    ))
    .await;

    finalize_copper_archive(
        tmp.path(),
        &session_id.to_string(),
        &ObservabilityCopperConfig {
            keep_local_after_upload: false,
            ..ObservabilityCopperConfig::default()
        },
        client,
    )
    .await
    .expect("finalizer soft-fails rather than returning errors");

    let rows = roz_db::session_artifacts::list_by_session(&harness.pool, harness.tenant_id, session_id)
        .await
        .expect("list_by_session");
    assert_eq!(rows.len(), 2, "one row per copper segment");

    let by_path: BTreeMap<_, _> = rows.iter().map(|row| (row.path.as_str(), row)).collect();
    for (path, bytes) in expected {
        let row = by_path.get(path.as_str()).expect("row path should match segment name");
        assert_eq!(row.artifact_type, "copper");
        assert_eq!(row.digest_sha256, sha256(&bytes));
    }
}

#[tokio::test]
#[ignore = "requires ArtifactService/Postgres loopback; routed by ci-integration nextest"]
async fn successful_copper_archive_cleanup_removes_session_dir() {
    let harness = setup_db_harness().await;
    let tmp = TempDir::new().expect("tempdir");
    let session_id = Uuid::new_v4();
    let _expected = write_copper_segments(tmp.path(), session_id).await;
    let session_dir = tmp.path().join("sessions").join(session_id.to_string());
    let client = spawn_client(LoopbackArtifactService::success(
        harness.pool.clone(),
        harness.tenant_id,
    ))
    .await;

    finalize_copper_archive(
        tmp.path(),
        &session_id.to_string(),
        &ObservabilityCopperConfig {
            keep_local_after_upload: false,
            ..ObservabilityCopperConfig::default()
        },
        client,
    )
    .await
    .expect("finalizer should complete");

    assert!(
        !session_dir.exists(),
        "successful upload should remove local session dir"
    );
}

#[tokio::test]
#[ignore = "requires ArtifactService/Postgres loopback; routed by ci-integration nextest"]
async fn soft_failed_copper_archive_retains_session_dir() {
    let tmp = TempDir::new().expect("tempdir");
    let session_id = Uuid::new_v4();
    let _expected = write_copper_segments(tmp.path(), session_id).await;
    let session_dir = tmp.path().join("sessions").join(session_id.to_string());
    let client = spawn_client(LoopbackArtifactService::soft_fail("session_1.copper")).await;

    finalize_copper_archive(
        tmp.path(),
        &session_id.to_string(),
        &ObservabilityCopperConfig {
            keep_local_after_upload: false,
            ..ObservabilityCopperConfig::default()
        },
        client,
    )
    .await
    .expect("finalizer soft-fails as Ok(())");

    assert!(
        session_dir.exists(),
        "soft-failed upload should retain local session dir"
    );
}

async fn setup_db_harness() -> DbHarness {
    let tenant_id = Uuid::new_v4();
    let guard = roz_test::pg_container().await;
    let url = guard.url().to_string();
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrations");

    let slug = format!("copper-finalize-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(&pool, "Copper Finalize Test", &slug, "personal")
        .await
        .expect("tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("pin tenant id");

    DbHarness {
        pool,
        _pg: guard,
        tenant_id,
    }
}

async fn spawn_client(service: LoopbackArtifactService) -> ArtifactServiceClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback artifact service");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let identity = AuthIdentity::User {
        user_id: "user:test".into(),
        org_id: None,
        tenant_id: TenantId::new(service.tenant_id),
        role: Role::Admin,
    };
    let server = ArtifactServiceServer::with_interceptor(service, move |mut req: Request<()>| {
        req.extensions_mut().insert(identity.clone());
        Ok(req)
    });

    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming(incoming)
            .await
            .expect("serve artifact service");
    });

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));
    for _ in 0..40 {
        if let Ok(channel) = endpoint.clone().connect().await {
            return ArtifactServiceClient::new(channel);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("artifact service did not accept loopback connection");
}

async fn write_copper_segments(data_dir: &std::path::Path, session_id: Uuid) -> BTreeMap<String, Vec<u8>> {
    let session_dir = data_dir.join("sessions").join(session_id.to_string());
    tokio::fs::create_dir_all(&session_dir).await.expect("session dir");
    let segments = BTreeMap::from([
        ("session_0.copper".to_string(), b"copper-segment-zero".to_vec()),
        ("session_1.copper".to_string(), b"copper-segment-one".to_vec()),
    ]);
    for (path, bytes) in &segments {
        tokio::fs::write(session_dir.join(path), bytes)
            .await
            .expect("write segment");
    }
    segments
}

fn verify_upload(metadata: &UploadArtifactMetadata, bytes: &[u8]) -> Result<(), Status> {
    if metadata.artifact_type != "copper" {
        return Err(Status::invalid_argument("expected copper artifact"));
    }
    if metadata.size_bytes != bytes.len() as u64 {
        return Err(Status::invalid_argument("size mismatch"));
    }
    let digest = sha256(bytes);
    if metadata.digest_sha256 != digest {
        return Err(Status::invalid_argument("digest mismatch"));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}
