//! Phase 26.8 SC3/SC5/SC6/SC7: end-to-end integration tests for
//! [`roz_worker::ulog_archive::finalize_ulog_archive`] against:
//!
//!   1. a mock [`roz_mavlink::MavlinkBackend`] constructed via
//!      [`MavlinkBackend::new_for_tests`] (feature `test-helpers`; see
//!      `crates/roz-mavlink/src/backend.rs`), and
//!   2. a mock [`ArtifactService`] running in-process over a tonic loopback
//!      socket (`127.0.0.1:0` → `ArtifactServiceClient::connect`).
//!
//! The mock MAVLink side is driven by [`common::mock_log_transport::MockLogTransport`]
//! — a copy of the roz-mavlink mock — replaying the FC side of the LOG_*
//! protocol against the checked-in PX4 ULG fixture. A replay task spawned
//! per test reads client → FC frames from the backend's outbound receiver
//! and broadcasts FC → client frames to the backend's log subscribers.
//!
//! # Phase 27 SITL swap point
//!
//! When Phase 27 lands, the `MavlinkBackend::new_for_tests(...)` call is
//! replaced with `MavlinkBackend::new_udp_in("127.0.0.1:14540", ...)`
//! against a real PX4 SITL container. The test assertions (upload metadata
//! + server-side digest/size echo + `LOG_ERASE` observation on the
//! outbound channel) remain valid; only the harness swaps. No test body
//! changes required.
//!
//! # Running
//! ```
//! cargo test -p roz-worker --test ulog_session_finalize_integration -- --test-threads=1
//! ```

#![allow(
    clippy::too_many_lines,
    reason = "integration tests carry unavoidable harness scaffolding"
)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mavlink::common::MavMessage;
use roz_mavlink::{AutopilotHint, MavlinkBackend};
use roz_worker::roz_v1::artifact_service_client::ArtifactServiceClient;
use roz_worker::roz_v1::artifact_service_server::{ArtifactService, ArtifactServiceServer};
use roz_worker::roz_v1::{
    DownloadArtifactChunk, DownloadArtifactRequest, ListSessionArtifactsRequest, ListSessionArtifactsResponse,
    UploadArtifactMetadata, UploadArtifactRequest, UploadArtifactResponse, upload_artifact_request,
};
use roz_worker::ulog_archive::finalize_ulog_archive;
use roz_worker::ulog_config::UlogConfig;
use tokio::sync::{broadcast, mpsc};
use tonic::{Request, Response, Status, Streaming};

use crate::common::mock_log_transport::MockLogTransport;

// ---------------------------------------------------------------------------
// Mock ArtifactService — records the streamed upload and returns a canned
// response (artifact_id + size echo).
// ---------------------------------------------------------------------------

/// Behaviour knob for the mock: succeed (echo received total bytes) or fail.
#[derive(Clone)]
enum MockUploadOutcome {
    /// Succeed; respond with `artifact_id` and server-echo of received total bytes.
    Success { artifact_id: String },
    /// Fail the upload with `Status::internal("simulated upload failure")`.
    InternalError,
}

struct MockArtifactService {
    received_metadata: Arc<Mutex<Option<UploadArtifactMetadata>>>,
    received_total_bytes: Arc<Mutex<u64>>,
    outcome: MockUploadOutcome,
}

impl MockArtifactService {
    fn new(outcome: MockUploadOutcome) -> Self {
        Self {
            received_metadata: Arc::new(Mutex::new(None)),
            received_total_bytes: Arc::new(Mutex::new(0)),
            outcome,
        }
    }
}

#[tonic::async_trait]
impl ArtifactService for MockArtifactService {
    async fn upload_artifact(
        &self,
        request: Request<Streaming<UploadArtifactRequest>>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let mut stream = request.into_inner();
        let mut total_bytes: u64 = 0;

        while let Some(frame) = stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("stream error: {e}")))?
        {
            match frame.payload {
                Some(upload_artifact_request::Payload::Metadata(md)) => {
                    *self.received_metadata.lock().expect("lock") = Some(md);
                }
                Some(upload_artifact_request::Payload::Chunk(ck)) => {
                    total_bytes += ck.data.len() as u64;
                }
                None => {}
            }
        }

        *self.received_total_bytes.lock().expect("lock") = total_bytes;

        match &self.outcome {
            MockUploadOutcome::Success { artifact_id } => Ok(Response::new(UploadArtifactResponse {
                artifact_id: artifact_id.clone(),
                size_bytes: total_bytes,
            })),
            MockUploadOutcome::InternalError => Err(Status::internal("simulated upload failure")),
        }
    }

    async fn download_artifact(
        &self,
        _request: Request<DownloadArtifactRequest>,
    ) -> Result<Response<Self::DownloadArtifactStream>, Status> {
        Err(Status::unimplemented("mock does not implement download"))
    }

    type DownloadArtifactStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<DownloadArtifactChunk, Status>> + Send + 'static>>;

    async fn list_session_artifacts(
        &self,
        _request: Request<ListSessionArtifactsRequest>,
    ) -> Result<Response<ListSessionArtifactsResponse>, Status> {
        Err(Status::unimplemented("mock does not implement list"))
    }
}

// ---------------------------------------------------------------------------
// Harness: spawn mock server, construct mock backend + replay task, return
// everything the test needs.
// ---------------------------------------------------------------------------

struct Harness {
    backend: Arc<MavlinkBackend>,
    client: ArtifactServiceClient<tonic::transport::Channel>,
    outbound_rx: mpsc::Receiver<MavMessage>,
    // Kept alive so the log broadcast channel stays open for the replay task.
    log_tx: broadcast::Sender<MavMessage>,
    received_metadata: Arc<Mutex<Option<UploadArtifactMetadata>>>,
    received_total_bytes: Arc<Mutex<u64>>,
    _replay_task: Option<tokio::task::JoinHandle<()>>,
    _server_task: tokio::task::JoinHandle<()>,
}

/// Build a harness wired to the given mock transport and upload outcome.
/// `autopilot_hint` selects the FC family the backend pretends to be.
async fn setup_harness(
    mock_transport: Option<MockLogTransport>,
    outcome: MockUploadOutcome,
    autopilot_hint: AutopilotHint,
) -> Harness {
    // --- Mock ArtifactService on an ephemeral port.
    let service = MockArtifactService::new(outcome);
    let received_metadata = service.received_metadata.clone();
    let received_total_bytes = service.received_total_bytes.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock artifact server");
    let addr: SocketAddr = listener.local_addr().expect("mock server local_addr");

    let server = ArtifactServiceServer::new(service);
    let server_task = tokio::spawn(async move {
        let stream = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming(stream)
            .await;
    });

    // --- Client that dials the mock server (retry loop for bind race).
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));
    let mut client = None;
    for _ in 0..40 {
        if let Ok(c) = endpoint.clone().connect().await {
            client = Some(ArtifactServiceClient::new(c));
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let client = client.expect("mock server must accept connection");

    // --- Mock MavlinkBackend: wire outbound mpsc + log broadcast channels.
    //
    // Replay task is NOT spawned here. The `outbound_rx` is moved into
    // `spawn_replay_with_observer` by the caller so each test can control
    // whether frames are driven against a mock transport or merely observed.
    //
    // `mock_transport` is accepted here for symmetry with the replay API but
    // is currently unused at harness-construction time. Suppress the
    // unused-param warning explicitly.
    let _ = mock_transport;
    let (out_tx, out_rx) = mpsc::channel::<MavMessage>(128);
    let (log_tx, _) = broadcast::channel::<MavMessage>(128);
    let backend = MavlinkBackend::new_for_tests(out_tx, log_tx.clone(), autopilot_hint);

    Harness {
        backend,
        client,
        outbound_rx: out_rx,
        log_tx,
        received_metadata,
        received_total_bytes,
        _replay_task: None,
        _server_task: server_task,
    }
}

/// Drive the mock: read a single frame from `out_rx`, forward it to
/// `observed_tx`, then produce replies from the mock and broadcast them on
/// `log_tx`. Returns `false` when the outbound channel closes.
async fn drive_one_frame(
    out_rx: &mut mpsc::Receiver<MavMessage>,
    log_tx: &broadcast::Sender<MavMessage>,
    mock: &mut MockLogTransport,
    observed_tx: &mpsc::Sender<MavMessage>,
) -> bool {
    match out_rx.recv().await {
        Some(msg) => {
            // Replies MAY depend on the incoming frame — drive the mock
            // BEFORE forwarding to the observer so the clone happens after
            // mock.drive_once borrows the frame.
            let replies = mock.drive_once(&msg);
            let _ = observed_tx.send(msg).await;
            for reply in replies {
                let _ = log_tx.send(reply);
            }
            true
        }
        None => false,
    }
}

/// Spawn the replay + observation task. Returns a JoinHandle + a receiver
/// that yields every MAVLink frame the backend sent outbound (for post-test
/// assertions on LOG_REQUEST_END, LOG_ERASE, etc.).
fn spawn_replay_with_observer(
    mut out_rx: mpsc::Receiver<MavMessage>,
    log_tx: broadcast::Sender<MavMessage>,
    mock: Option<MockLogTransport>,
) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<MavMessage>) {
    let (observed_tx, observed_rx) = mpsc::channel::<MavMessage>(256);
    let handle = tokio::spawn(async move {
        match mock {
            Some(mut mock) => {
                while drive_one_frame(&mut out_rx, &log_tx, &mut mock, &observed_tx).await {
                    // loop
                }
            }
            None => {
                // No mock: just observe outbound without generating replies.
                while let Some(msg) = out_rx.recv().await {
                    let _ = observed_tx.send(msg).await;
                }
            }
        }
    });
    (handle, observed_rx)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SC3 + SC6 happy path: PX4 backend + successful upload → LOG_ERASE emitted.
#[tokio::test]
async fn finalize_happy_path_uploads_ulog_and_erases_fc() {
    let mock = MockLogTransport::load_from_default_fixture().expect("fixture");
    let fixture_len = mock.fixture_bytes().len();

    let mut harness = setup_harness(
        None, // replay wired separately below
        MockUploadOutcome::Success {
            artifact_id: "artifact-happy-path".into(),
        },
        AutopilotHint::Px4,
    )
    .await;

    // Swap out the outbound_rx (Harness owns it) into the replay task.
    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), Some(mock));

    let config = UlogConfig {
        enabled: true,
        download_timeout_secs: 5,
        keep_fc_copy: false,
    };

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-happy-path",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never returns Err in scope");

    assert_eq!(
        result.as_deref(),
        Some("artifact-happy-path"),
        "must return server-issued artifact_id"
    );

    // Drop the backend so the backend's outbound sender closes. The
    // LogDownloader inside finalize_ulog_archive has already dropped its
    // cloned sender, so once we drop the backend the mpsc receiver in the
    // replay task observes EOF and the task exits naturally after draining
    // the buffered frames. Await the handle rather than aborting — this
    // guarantees LOG_ERASE has been forwarded to the observer channel
    // before we read it below (no race window, no sleep heuristic).
    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    // --- Assert upload metadata ---
    let md = harness
        .received_metadata
        .lock()
        .expect("lock")
        .clone()
        .expect("server must have received metadata");
    assert_eq!(md.session_id, "session-happy-path");
    assert_eq!(md.artifact_type, "ulog");
    assert_eq!(md.content_type, "application/vnd.px4.ulg");
    assert_eq!(md.path, "ulog/session-happy-path.ulg");
    assert_eq!(
        md.size_bytes as usize, fixture_len,
        "declared size must equal fixture size"
    );
    assert_eq!(md.digest_sha256.len(), 32, "digest must be 32 bytes");

    // --- Assert server received byte total ---
    let received_total = *harness.received_total_bytes.lock().expect("lock");
    assert_eq!(
        received_total as usize, fixture_len,
        "server must have streamed the full fixture"
    );

    // --- Assert LOG_ERASE observed on outbound (D-06) ---
    let mut saw_log_request_list = false;
    let mut saw_log_request_data = false;
    let mut saw_log_request_end = false;
    let mut saw_log_erase = false;
    while let Ok(msg) = observed_rx.try_recv() {
        match msg {
            MavMessage::LOG_REQUEST_LIST(_) => saw_log_request_list = true,
            MavMessage::LOG_REQUEST_DATA(_) => saw_log_request_data = true,
            MavMessage::LOG_REQUEST_END(_) => saw_log_request_end = true,
            MavMessage::LOG_ERASE(_) => saw_log_erase = true,
            _ => {}
        }
    }
    assert!(saw_log_request_list, "downloader must have sent LOG_REQUEST_LIST");
    assert!(saw_log_request_data, "downloader must have sent LOG_REQUEST_DATA");
    assert!(saw_log_request_end, "downloader must have sent LOG_REQUEST_END");
    assert!(
        saw_log_erase,
        "D-06: LOG_ERASE must be emitted after verified upload (keep_fc_copy=false)"
    );
}

/// SC6 retention path: keep_fc_copy=true skips LOG_ERASE even on success.
#[tokio::test]
async fn finalize_keep_fc_copy_true_skips_erase() {
    let mock = MockLogTransport::load_from_default_fixture().expect("fixture");

    let mut harness = setup_harness(
        None,
        MockUploadOutcome::Success {
            artifact_id: "artifact-keep-fc".into(),
        },
        AutopilotHint::Px4,
    )
    .await;

    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), Some(mock));

    let config = UlogConfig {
        enabled: true,
        download_timeout_secs: 5,
        keep_fc_copy: true,
    };

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-keep-fc",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never Errs");

    assert_eq!(result.as_deref(), Some("artifact-keep-fc"));

    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    let mut saw_log_erase = false;
    let mut saw_log_request_end = false;
    while let Ok(msg) = observed_rx.try_recv() {
        match msg {
            MavMessage::LOG_ERASE(_) => saw_log_erase = true,
            MavMessage::LOG_REQUEST_END(_) => saw_log_request_end = true,
            _ => {}
        }
    }
    assert!(saw_log_request_end, "downloader still must have sent LOG_REQUEST_END");
    assert!(
        !saw_log_erase,
        "keep_fc_copy=true must NOT emit LOG_ERASE (D-06 retention path)"
    );
}

/// D-11 gate: ArduPilot backend exits early with Ok(None) and no protocol traffic.
#[tokio::test]
async fn finalize_autopilot_not_px4_skips_entirely() {
    let mut harness = setup_harness(
        None,
        MockUploadOutcome::Success {
            artifact_id: "artifact-never-sent".into(),
        },
        AutopilotHint::ArduCopter,
    )
    .await;

    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    // No replay task needed (no protocol exchange expected); spawn one anyway
    // to drain outbound so we can assert emptiness at the end.
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), None);

    let config = UlogConfig::default();

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-ardupilot",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never Errs");

    assert_eq!(result, None, "ArduPilot backend must return Ok(None)");

    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    let mut saw_any = false;
    while observed_rx.try_recv().is_ok() {
        saw_any = true;
    }
    assert!(!saw_any, "D-11: non-PX4 autopilot must emit ZERO MAVLink frames");

    let md = harness.received_metadata.lock().expect("lock").clone();
    assert!(md.is_none(), "no upload must have started");
}

/// D-08 opt-out: UlogConfig.enabled=false returns Ok(None) silently.
#[tokio::test]
async fn finalize_disabled_returns_ok_none_silently() {
    let mut harness = setup_harness(
        None,
        MockUploadOutcome::Success {
            artifact_id: "artifact-never-sent".into(),
        },
        AutopilotHint::Px4,
    )
    .await;

    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), None);

    let config = UlogConfig {
        enabled: false,
        download_timeout_secs: 5,
        keep_fc_copy: false,
    };

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-disabled",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never Errs");

    assert_eq!(result, None, "enabled=false must return Ok(None)");

    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    let mut saw_any = false;
    while observed_rx.try_recv().is_ok() {
        saw_any = true;
    }
    assert!(!saw_any, "D-08: disabled config must emit ZERO MAVLink frames");

    let md = harness.received_metadata.lock().expect("lock").clone();
    assert!(md.is_none(), "no upload must have started");
}

/// D-06 correctness: upload failure must NOT trigger LOG_ERASE.
#[tokio::test]
async fn finalize_upload_fails_does_not_erase() {
    let mock = MockLogTransport::load_from_default_fixture().expect("fixture");

    let mut harness = setup_harness(None, MockUploadOutcome::InternalError, AutopilotHint::Px4).await;

    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), Some(mock));

    let config = UlogConfig {
        enabled: true,
        download_timeout_secs: 5,
        keep_fc_copy: false,
    };

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-upload-fails",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never Errs (soft-fails to Ok(None))");

    assert_eq!(result, None, "upload failure must surface as Ok(None) (soft-fail)");

    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    let mut saw_log_erase = false;
    let mut saw_log_request_end = false;
    while let Ok(msg) = observed_rx.try_recv() {
        match msg {
            MavMessage::LOG_ERASE(_) => saw_log_erase = true,
            MavMessage::LOG_REQUEST_END(_) => saw_log_request_end = true,
            _ => {}
        }
    }
    assert!(
        saw_log_request_end,
        "LogDownloader Drop-guard still must emit LOG_REQUEST_END"
    );
    assert!(
        !saw_log_erase,
        "T-26.8.07-01: upload failure must NEVER trigger LOG_ERASE"
    );
}

/// D-09 no-logs path: mock reports num_logs=0; no upload, soft-fail Ok(None).
#[tokio::test]
async fn finalize_no_logs_available_warns_without_upload() {
    let mock = MockLogTransport::load_from_default_fixture()
        .expect("fixture")
        .with_num_logs_zero();

    let mut harness = setup_harness(
        None,
        MockUploadOutcome::Success {
            artifact_id: "artifact-never-sent".into(),
        },
        AutopilotHint::Px4,
    )
    .await;

    let out_rx = std::mem::replace(&mut harness.outbound_rx, mpsc::channel(1).1);
    let (replay_handle, mut observed_rx) = spawn_replay_with_observer(out_rx, harness.log_tx.clone(), Some(mock));

    let config = UlogConfig {
        enabled: true,
        download_timeout_secs: 2,
        keep_fc_copy: false,
    };

    let result = finalize_ulog_archive(
        harness.backend.clone(),
        "session-no-logs",
        &config,
        harness.client.clone(),
    )
    .await
    .expect("finalize never Errs");

    assert_eq!(result, None, "no logs on FC must surface as Ok(None)");

    drop(harness.backend);
    replay_handle.await.expect("replay task must exit cleanly");

    let md = harness.received_metadata.lock().expect("lock").clone();
    assert!(md.is_none(), "no upload must have started when FC reports num_logs=0");

    let mut saw_log_erase = false;
    while let Ok(msg) = observed_rx.try_recv() {
        if matches!(msg, MavMessage::LOG_ERASE(_)) {
            saw_log_erase = true;
        }
    }
    assert!(!saw_log_erase, "no_logs_available must NOT trigger LOG_ERASE");
}
