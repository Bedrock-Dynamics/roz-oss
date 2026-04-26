//! Phase 26.11 Plan 05: live `roz session export --bundle` CLI coverage.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use roz_server::grpc::roz_v1::artifact_service_server::ArtifactService;
use roz_server::grpc::roz_v1::artifact_service_server::ArtifactServiceServer;
use roz_server::grpc::roz_v1::observability_service_server::ObservabilityService;
use roz_server::grpc::roz_v1::observability_service_server::ObservabilityServiceServer;
use roz_server::grpc::roz_v1::{
    ArtifactSummary, DownloadArtifactChunk, DownloadArtifactRequest, ExportSessionChunk, ExportSessionRequest,
    ListSessionArtifactsRequest, ListSessionArtifactsResponse, ReindexAllRequest, ReindexProgress,
    ReindexSessionRequest, ReindexSessionResponse,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};
use uuid::Uuid;

const EXPECTED_AUTH: &str = "Bearer test-token";

#[derive(Clone)]
struct LoopbackServices {
    session_id: String,
    mcap_bytes: Arc<Vec<u8>>,
    artifacts: Arc<BTreeMap<String, ArtifactFixture>>,
}

#[derive(Clone)]
struct ArtifactFixture {
    artifact_id: String,
    artifact_type: String,
    path: String,
    content_type: String,
    bytes: Vec<u8>,
    digest_sha256: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct BundleManifest {
    files: Vec<BundleFile>,
}

#[derive(Debug, Deserialize)]
struct BundleFile {
    path: String,
    digest_sha256: String,
    size_bytes: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback gRPC services and CLI subprocess; routed by ci-integration nextest"]
async fn cli_bundle_export_includes_mcap_and_artifacts_with_verified_digests() {
    let session_id = Uuid::new_v4().to_string();
    let services = LoopbackServices::new(session_id.clone());
    let loopback_url = spawn_services(services).await;

    let tmp = TempDir::new().expect("tempdir");
    let bundle_path = tmp.path().join("session-bundle.tar");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_roz"));
    cmd.env("ROZ_API_URL", loopback_url);
    cmd.env("ROZ_API_KEY", "test-token");
    cmd.args([
        "session",
        "export",
        session_id.as_str(),
        "--format",
        "mcap",
        "--bundle",
        "--output",
        bundle_path.to_str().expect("utf8 path"),
    ]);
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::task::spawn_blocking(move || cmd.output()),
    )
    .await
    .expect("roz session export --bundle timed out")
    .expect("CLI subprocess task")
    .expect("run roz session export --bundle");
    assert!(
        output.status.success(),
        "CLI failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let bundle = std::fs::read(&bundle_path).expect("bundle tar");
    let (paths, files) = read_tar_entries(&bundle);
    assert_eq!(paths.first().map(String::as_str), Some("manifest.json"));
    assert_eq!(
        paths.iter().filter(|path| path.as_str() == "manifest.json").count(),
        1,
        "manifest.json must not be duplicated"
    );

    let manifest: BundleManifest =
        serde_json::from_slice(files.get("manifest.json").expect("manifest.json bytes")).expect("manifest json");
    let manifest_paths: BTreeSet<_> = manifest.files.iter().map(|file| file.path.as_str()).collect();
    let mcap_paths: Vec<_> = manifest_paths
        .iter()
        .copied()
        .filter(|path| path.starts_with("mcap/"))
        .collect();
    assert_eq!(mcap_paths.len(), 1, "manifest should include exactly one MCAP path");
    assert!(manifest_paths.contains("copper/session_0.copper"));
    assert!(manifest_paths.contains("copper/session_1.copper"));

    for file in &manifest.files {
        let bytes = files
            .get(file.path.as_str())
            .unwrap_or_else(|| panic!("tar missing manifest-listed file {}", file.path));
        assert_eq!(bytes.len() as u64, file.size_bytes, "size_bytes for {}", file.path);
        assert_eq!(
            hex::encode(sha256(bytes)),
            file.digest_sha256,
            "digest_sha256 for {}",
            file.path
        );
    }

    let mcap_bytes = files.get(mcap_paths[0]).expect("mcap entry bytes");
    let _messages: Vec<_> = mcap::MessageStream::new(mcap_bytes.as_slice())
        .expect("bundle MCAP should be readable")
        .collect::<Result<Vec<_>, _>>()
        .expect("read MCAP messages");
}

impl LoopbackServices {
    fn new(session_id: String) -> Self {
        let artifacts = [
            artifact("artifact-session-0", "session_0.copper", b"copper-segment-zero"),
            artifact("artifact-session-1", "session_1.copper", b"copper-segment-one"),
        ]
        .into_iter()
        .map(|fixture| (fixture.artifact_id.clone(), fixture))
        .collect();
        Self {
            session_id,
            mcap_bytes: Arc::new(write_empty_mcap()),
            artifacts: Arc::new(artifacts),
        }
    }
}

#[tonic::async_trait]
impl ObservabilityService for LoopbackServices {
    type ExportSessionStream = ReceiverStream<Result<ExportSessionChunk, Status>>;
    type ReindexAllStream = ReceiverStream<Result<ReindexProgress, Status>>;

    async fn export_session(
        &self,
        request: Request<ExportSessionRequest>,
    ) -> Result<Response<Self::ExportSessionStream>, Status> {
        require_auth(&request)?;
        let request = request.into_inner();
        if request.session_id != self.session_id {
            return Err(Status::not_found("session not found"));
        }
        if request.time_range.is_some() {
            return Err(Status::invalid_argument("bundle test expects full-session export"));
        }
        let chunk = ExportSessionChunk {
            data: self.mcap_bytes.as_ref().clone(),
            archive_status: Some("finalized".to_string()),
            rollover_index: Some(0),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(chunk)).await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn reindex_session(
        &self,
        _request: Request<ReindexSessionRequest>,
    ) -> Result<Response<ReindexSessionResponse>, Status> {
        Err(Status::unimplemented("not used by bundle export test"))
    }

    async fn reindex_all(
        &self,
        _request: Request<ReindexAllRequest>,
    ) -> Result<Response<Self::ReindexAllStream>, Status> {
        Err(Status::unimplemented("not used by bundle export test"))
    }
}

#[tonic::async_trait]
impl ArtifactService for LoopbackServices {
    type DownloadArtifactStream = ReceiverStream<Result<DownloadArtifactChunk, Status>>;

    async fn upload_artifact(
        &self,
        _request: Request<tonic::Streaming<roz_server::grpc::roz_v1::UploadArtifactRequest>>,
    ) -> Result<Response<roz_server::grpc::roz_v1::UploadArtifactResponse>, Status> {
        Err(Status::unimplemented("not used by bundle export test"))
    }

    async fn download_artifact(
        &self,
        request: Request<DownloadArtifactRequest>,
    ) -> Result<Response<Self::DownloadArtifactStream>, Status> {
        require_auth(&request)?;
        let artifact_id = request.into_inner().artifact_id;
        let fixture = self
            .artifacts
            .get(&artifact_id)
            .ok_or_else(|| Status::not_found("artifact not found"))?
            .clone();
        let chunk = DownloadArtifactChunk {
            data: fixture.bytes,
            digest_sha256: Some(fixture.digest_sha256),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(chunk)).await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn list_session_artifacts(
        &self,
        request: Request<ListSessionArtifactsRequest>,
    ) -> Result<Response<ListSessionArtifactsResponse>, Status> {
        require_auth(&request)?;
        let request = request.into_inner();
        if request.session_id != self.session_id {
            return Err(Status::not_found("session not found"));
        }
        let artifacts = self
            .artifacts
            .values()
            .map(|fixture| ArtifactSummary {
                artifact_id: fixture.artifact_id.clone(),
                artifact_type: fixture.artifact_type.clone(),
                path: fixture.path.clone(),
                digest_sha256: fixture.digest_sha256.clone(),
                size_bytes: fixture.bytes.len() as u64,
                content_type: fixture.content_type.clone(),
            })
            .collect();
        Ok(Response::new(ListSessionArtifactsResponse { artifacts }))
    }
}

async fn spawn_services(services: LoopbackServices) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback services");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let observability = ObservabilityServiceServer::new(services.clone());
    let artifacts = ArtifactServiceServer::new(services);
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        tonic::transport::Server::builder()
            .add_service(observability)
            .add_service(artifacts)
            .serve_with_incoming(incoming)
            .await
            .expect("serve loopback services");
    });
    wait_for_port(addr).await;
    format!("http://{addr}")
}

async fn wait_for_port(addr: SocketAddr) {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));
    for _ in 0..40 {
        if endpoint.clone().connect().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("loopback services did not accept a connection");
}

fn require_auth<T>(request: &Request<T>) -> Result<(), Status> {
    let auth = request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if auth == Some(EXPECTED_AUTH) {
        Ok(())
    } else {
        Err(Status::unauthenticated("missing expected authorization metadata"))
    }
}

fn artifact(artifact_id: &str, path: &str, bytes: &[u8]) -> ArtifactFixture {
    ArtifactFixture {
        artifact_id: artifact_id.to_string(),
        artifact_type: "copper".to_string(),
        path: path.to_string(),
        content_type: "application/vnd.copper-log".to_string(),
        bytes: bytes.to_vec(),
        digest_sha256: sha256(bytes),
    }
}

fn write_empty_mcap() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut writer = mcap::Writer::new(std::io::Cursor::new(&mut bytes)).expect("mcap writer");
        writer.finish().expect("finish mcap");
    }
    bytes
}

fn read_tar_entries(bundle: &[u8]) -> (Vec<String>, BTreeMap<String, Vec<u8>>) {
    let mut archive = tar::Archive::new(std::io::Cursor::new(bundle));
    let mut paths = Vec::new();
    let mut files = BTreeMap::new();
    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let path = entry.path().expect("entry path").to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("entry bytes");
        paths.push(path.clone());
        files.insert(path, bytes);
    }
    (paths, files)
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}
