//! Phase 26.7 SC6: `ArtifactService::{UploadArtifact, DownloadArtifact}`
//! integration tests over a real tonic channel with axum-injected AuthIdentity.
//!
//! Template: `crates/roz-server/tests/observability_export_grpc.rs`
//! (Per 26.7-RESEARCH.md Discrepancy 3, this is the correct template —
//! NOT `mcap_camera_roundtrip.rs` which drives `WriterActor` directly without
//! opening a tonic channel.)
//!
//! # Coverage
//! - `upload_and_download_roundtrip` — happy path (128 KB fixture, digest round-trip).
//! - `tampered_digest_rejected_and_cleaned_up` — D-34 negative path.
//! - `list_session_artifacts_empty_for_new_session` — ListSessionArtifacts smoke.
//!
//! # Running
//! ```
//! cargo test -p roz-server --test artifact_copper_roundtrip \
//!   --features test-helpers -- --ignored --test-threads=1
//! ```

#![allow(clippy::too_many_lines, reason = "integration tests carry unavoidable scaffolding")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use roz_core::auth::{AuthIdentity, Role, TenantId};
use roz_db::{create_pool, run_migrations};
use roz_server::grpc::artifacts::ArtifactServiceImpl;
use roz_server::grpc::roz_v1::artifact_service_client::ArtifactServiceClient;
use roz_server::grpc::roz_v1::artifact_service_server::ArtifactServiceServer;
use roz_server::grpc::roz_v1::{
    DownloadArtifactRequest, ListSessionArtifactsRequest, UploadArtifactChunk, UploadArtifactMetadata,
    UploadArtifactRequest, upload_artifact_request,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Middleware: inject AuthIdentity into Request::extensions (mirrors
// observability_export_grpc.rs).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct InjectState {
    identity: AuthIdentity,
}

async fn inject_extensions_middleware(
    axum::extract::State(state): axum::extract::State<InjectState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(state.identity.clone());
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    pool: PgPool,
    _pg: roz_test::PgGuard,
    tenant_id: Uuid,
    artifact_dir: PathBuf,
    addr: SocketAddr,
    _artifact_tmp: TempDir,
}

impl Harness {
    async fn client(&self) -> ArtifactServiceClient<tonic::transport::Channel> {
        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{}", self.addr))
            .expect("endpoint")
            .connect_timeout(Duration::from_secs(5));
        for _ in 0..40 {
            if let Ok(c) = endpoint.clone().connect().await {
                return ArtifactServiceClient::new(c);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("connect failed after retries");
    }
}

async fn setup_harness() -> Harness {
    let tenant_id = Uuid::new_v4();

    // Postgres via testcontainers.
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    // Tenant row for the caller — FK for roz_session_artifacts.
    let slug = format!("artifact-test-{}", Uuid::new_v4());
    let _ = roz_db::tenant::create_tenant(&pool, "Artifact Test", &slug, "personal")
        .await
        .expect("tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("update tenant id");

    // Canonical artifact dir.
    let artifact_tmp = tempfile::tempdir().expect("artifact tempdir");
    let artifact_dir = std::fs::canonicalize(artifact_tmp.path()).expect("canonicalize artifact dir");

    // Service + middleware-wrapped router.
    let svc = ArtifactServiceImpl::new(pool.clone(), artifact_dir.clone());
    let server = ArtifactServiceServer::new(svc);
    let identity = AuthIdentity::User {
        user_id: "user:test".into(),
        org_id: None,
        tenant_id: TenantId::new(tenant_id),
        role: Role::Admin,
    };
    let inject_state = InjectState { identity };
    let router =
        tonic::service::Routes::new(server)
            .prepare()
            .into_axum_router()
            .layer(axum::middleware::from_fn_with_state(
                inject_state,
                inject_extensions_middleware,
            ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc serve");
    });

    Harness {
        pool,
        _pg: guard,
        tenant_id,
        artifact_dir,
        addr,
        _artifact_tmp: artifact_tmp,
    }
}

/// Build a client-streaming upload: one Metadata frame + N Chunk frames
/// split at 1 MiB boundaries (matches the server's UPLOAD_CHUNK_SIZE).
fn build_upload_stream(
    session_id: Uuid,
    artifact_type: &str,
    path: &str,
    bytes: Vec<u8>,
    digest: Vec<u8>,
    content_type: &str,
) -> ReceiverStream<UploadArtifactRequest> {
    let size_bytes = bytes.len() as u64;
    let (tx, rx) = mpsc::channel::<UploadArtifactRequest>(8);
    let artifact_type = artifact_type.to_string();
    let path = path.to_string();
    let content_type = content_type.to_string();
    tokio::spawn(async move {
        let _ = tx
            .send(UploadArtifactRequest {
                payload: Some(upload_artifact_request::Payload::Metadata(UploadArtifactMetadata {
                    session_id: session_id.to_string(),
                    artifact_type,
                    path,
                    size_bytes,
                    digest_sha256: digest,
                    content_type,
                })),
            })
            .await;
        for chunk in bytes.chunks(1024 * 1024) {
            let _ = tx
                .send(UploadArtifactRequest {
                    payload: Some(upload_artifact_request::Payload::Chunk(UploadArtifactChunk {
                        data: chunk.to_vec(),
                    })),
                })
                .await;
        }
    });
    ReceiverStream::new(rx)
}

// ---------------------------------------------------------------------------
// Happy path: upload + download + digest round-trip.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn upload_and_download_roundtrip() {
    let harness = setup_harness().await;
    let mut client = harness.client().await;

    // 128 KB deterministic fixture.
    let fixture: Vec<u8> = (0..131_072_u32).map(|i| (i % 256) as u8).collect();
    let digest = {
        let mut h = Sha256::new();
        h.update(&fixture);
        h.finalize().to_vec()
    };
    let session_id = Uuid::new_v4();

    // ---- Upload.
    let upload_stream = build_upload_stream(
        session_id,
        "copper",
        "session_0.copper",
        fixture.clone(),
        digest.clone(),
        "application/vnd.copper-log",
    );
    let response = client
        .upload_artifact(tonic::Request::new(upload_stream))
        .await
        .expect("upload")
        .into_inner();

    assert_eq!(response.size_bytes as usize, fixture.len());
    let artifact_id = Uuid::parse_str(&response.artifact_id).expect("uuid");

    // ---- Row assertion.
    let row = roz_db::session_artifacts::fetch_by_id(&harness.pool, harness.tenant_id, artifact_id)
        .await
        .expect("fetch")
        .expect("row present");
    assert_eq!(row.artifact_type, "copper");
    assert_eq!(row.digest_sha256, digest);
    assert_eq!(row.size_bytes as usize, fixture.len());
    assert_eq!(row.tenant_id, harness.tenant_id);
    assert_eq!(row.session_id, session_id);

    // ---- File-on-disk assertion: exactly one file under artifact_dir and
    // no leftover *.tmp (rename succeeded).
    let mut found_count = 0usize;
    let mut entries = tokio::fs::read_dir(
        harness
            .artifact_dir
            .join(harness.tenant_id.to_string())
            .join(session_id.to_string()),
    )
    .await
    .expect("read_dir");
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        if entry.file_type().await.unwrap().is_file() {
            found_count += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "tmp should be renamed; found {name}");
        }
    }
    assert_eq!(found_count, 1);

    // ---- Download.
    let mut stream = client
        .download_artifact(DownloadArtifactRequest {
            artifact_id: artifact_id.to_string(),
        })
        .await
        .expect("download")
        .into_inner();

    let mut received = Vec::with_capacity(fixture.len());
    let mut final_digest: Option<Vec<u8>> = None;
    while let Some(chunk) = stream.message().await.expect("chunk") {
        received.extend_from_slice(&chunk.data);
        if let Some(d) = chunk.digest_sha256 {
            final_digest = Some(d);
        }
    }

    assert_eq!(received, fixture);
    assert_eq!(final_digest.expect("final digest present"), digest);
}

// ---------------------------------------------------------------------------
// D-34: tampered digest → INVALID_ARGUMENT and partial-file cleanup.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn tampered_digest_rejected_and_cleaned_up() {
    let harness = setup_harness().await;
    let mut client = harness.client().await;

    let fixture: Vec<u8> = (0..64_000_u32).map(|i| (i as u8).wrapping_mul(3)).collect();
    let digest_fake = vec![0u8; 32];
    let session_id = Uuid::new_v4();

    let upload_stream = build_upload_stream(
        session_id,
        "copper",
        "session_0.copper",
        fixture,
        digest_fake,
        "application/vnd.copper-log",
    );

    let result = client.upload_artifact(tonic::Request::new(upload_stream)).await;
    let status = result.expect_err("digest mismatch should reject");
    assert_eq!(
        status.code(),
        tonic::Code::InvalidArgument,
        "expected INVALID_ARGUMENT; got {}: {}",
        status.code(),
        status.message()
    );

    // Walk the artifact_dir; the tmp file + canonical file must both be absent.
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(&harness.artifact_dir, &mut files);
    assert!(
        files.is_empty(),
        "no files should remain under artifact_dir after digest mismatch; found {:?}",
        files
    );
}

// ---------------------------------------------------------------------------
// Smoke: ListSessionArtifacts returns empty for a brand-new session_id.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn list_session_artifacts_empty_for_new_session() {
    let harness = setup_harness().await;
    let mut client = harness.client().await;
    let random_session = Uuid::new_v4();

    let response = client
        .list_session_artifacts(ListSessionArtifactsRequest {
            session_id: random_session.to_string(),
        })
        .await
        .expect("list")
        .into_inner();

    assert!(response.artifacts.is_empty(), "empty session must have zero artifacts");
}
