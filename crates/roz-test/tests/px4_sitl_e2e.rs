//! Phase 27 — PX4/Substrate SITL vertical test.
//!
//! This test exercises the real Roz harness path for drone commands:
//! NATS `TaskInvocation` -> `roz-worker` subprocess -> OodaReAct agent loop ->
//! `flight_command` tool -> boot-scoped `MavlinkBackend` -> PX4 SITL.
//!
//! It is ignored because it requires Docker, the Substrate/PX4 simulator image,
//! and a `roz-worker` binary built with `--features test-fixtures`.

#![allow(
    clippy::too_many_lines,
    reason = "integration tests carry scenario-driver scaffolding"
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use prost::Message;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
use roz_core::embodiment::model::{EmbodimentFamily, EmbodimentModel, Link};
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, TaskResult, TaskStatusEvent};
use roz_nats::subjects::Subjects;
use roz_proto::roz_v1::{ReadinessState, TelemetryUpdate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use uuid::Uuid;

const TEST_WORKER_ID: &str = "phase27-px4-sitl-worker";
const TEST_TENANT_ID: &str = "phase27-tenant";
const TAKEOFF_ALTITUDE_M: f64 = 5.0;

struct WorkerProcess {
    child: Child,
    _data_dir: tempfile::TempDir,
    _cwd: tempfile::TempDir,
    logs: Arc<Mutex<String>>,
    mavlink_tlog_path: PathBuf,
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl WorkerProcess {
    fn logs(&self) -> String {
        self.logs.lock().expect("worker log buffer poisoned").clone()
    }

    fn mavlink_tlog_len(&self) -> u64 {
        std::fs::metadata(&self.mavlink_tlog_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }

    async fn mavlink_tlog_since(&self, offset: u64) -> Vec<u8> {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let bytes = std::fs::read(&self.mavlink_tlog_path).unwrap_or_default();
        let start = usize::try_from(offset).unwrap_or(bytes.len()).min(bytes.len());
        bytes[start..].to_vec()
    }
}

#[derive(Default)]
struct FixtureCaptureReport {
    bootstrapped: Vec<(String, usize)>,
    diffs: Vec<(String, usize, usize)>,
}

impl FixtureCaptureReport {
    fn record(&mut self, outcome: FixtureCaptureOutcome) {
        match outcome {
            FixtureCaptureOutcome::Disabled | FixtureCaptureOutcome::Matched => {}
            FixtureCaptureOutcome::Bootstrapped { rel_path, size } => {
                self.bootstrapped.push((rel_path, size));
            }
            FixtureCaptureOutcome::Diff {
                rel_path,
                committed_size,
                captured_size,
            } => {
                self.diffs.push((rel_path, committed_size, captured_size));
            }
        }
    }

    fn assert_clean(&self) {
        if self.bootstrapped.is_empty() && self.diffs.is_empty() {
            return;
        }

        let mut message = String::from("fixture capture completed with pending operator action");
        if !self.bootstrapped.is_empty() {
            message.push_str("\nbootstrapped missing fixtures:");
            for (rel_path, size) in &self.bootstrapped {
                message.push_str(&format!("\n  - {rel_path} ({size} bytes)"));
            }
        }
        if !self.diffs.is_empty() {
            message.push_str("\nfixtures differ from committed bytes:");
            for (rel_path, committed_size, captured_size) in &self.diffs {
                message.push_str(&format!(
                    "\n  - {rel_path} (committed {committed_size} bytes, captured {captured_size} bytes)"
                ));
            }
        }
        message.push_str(
            "\nInspect the captured fixtures, commit intentional changes under \
             crates/roz-mavlink/tests/fixtures/, then rerun with ROZ_PX4_CAPTURE_TLOG_FIXTURES=1 \
             to verify-only diff the committed bytes.",
        );
        panic!("{message}");
    }
}

enum FixtureCaptureOutcome {
    Disabled,
    Matched,
    Bootstrapped {
        rel_path: String,
        size: usize,
    },
    Diff {
        rel_path: String,
        committed_size: usize,
        captured_size: usize,
    },
}

struct ResultServer {
    url: String,
    rx: tokio::sync::mpsc::Receiver<TaskResult>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ResultServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "opt-in direct native-MAVLink worker E2E; set ROZ_RUN_NATIVE_PX4_MAVLINK_E2E=1"]
async fn px4_sitl_full_scenario() {
    if std::env::var("PX4_SITL_DISABLE").is_ok() {
        eprintln!("SKIP: PX4_SITL_DISABLE set");
        return;
    }
    if std::env::var("ROZ_RUN_NATIVE_PX4_MAVLINK_E2E").is_err() {
        eprintln!(
            "SKIP: direct native-MAVLink worker E2E not requested. The Substrate Docker simulator path is \
             covered by roz-local::env_start_px4_docker_wasm_velocity_flies_10m; this test requires a direct \
             FCU/SITL MAVLink endpoint."
        );
        return;
    }

    let worker_bin = cargo_bin("roz-worker");
    if !worker_bin.exists() {
        eprintln!(
            "SKIP: {} does not exist; build it first with `cargo build -p roz-worker --features test-fixtures`",
            worker_bin.display()
        );
        return;
    }

    let Some(mavlink_transport) = direct_mavlink_transport_or_skip() else {
        return;
    };
    let Some(nats) = start_nats_or_skip().await else {
        return;
    };

    let mut result_server = spawn_result_server().await.expect("result callback server");
    let nats_client = async_nats::connect(nats.url()).await.expect("connect NATS");
    let telemetry_subject = Subjects::telemetry_state(TEST_WORKER_ID).expect("telemetry subject");
    let mut telemetry_sub = nats_client
        .subscribe(telemetry_subject)
        .await
        .expect("subscribe telemetry");

    let worker = spawn_worker(&worker_bin, nats.url(), &result_server.url, &mavlink_transport)
        .await
        .expect("spawn worker subprocess");

    let mut fixture_capture = FixtureCaptureReport::default();
    let boot_tlog_start = worker.mavlink_tlog_len();
    let boot = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(120), |readiness| {
        readiness.heartbeat_alive
    })
    .await
    .expect("worker should publish MAVLink readiness telemetry");
    let readiness = boot.readiness.as_ref().expect("readiness present");
    assert!(readiness.heartbeat_alive, "PX4 heartbeat must be alive: {readiness:?}");
    if !readiness.ready_to_arm {
        capture_readiness_tlog(&mut fixture_capture, &worker, "not_ready", boot_tlog_start).await;
    }
    if !readiness.fully_operational {
        capture_readiness_tlog(&mut fixture_capture, &worker, "degraded", boot_tlog_start).await;
    }

    let runtime = drone_runtime();
    let manifest = empty_control_manifest();
    let arm_tlog_start = worker.mavlink_tlog_len();
    run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime.clone(),
        manifest.clone(),
        serde_json::json!({ "command": "arm" }),
    )
    .await;
    capture_compliance_tlog(&mut fixture_capture, &worker, "arm", arm_tlog_start).await;

    let takeoff_tlog_start = worker.mavlink_tlog_len();
    let takeoff = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime.clone(),
        manifest.clone(),
        serde_json::json!({ "command": "takeoff", "altitude_m": TAKEOFF_ALTITUDE_M }),
    )
    .await;
    capture_compliance_tlog(&mut fixture_capture, &worker, "takeoff", takeoff_tlog_start).await;
    assert_task_result_accepted("takeoff", &takeoff);

    let airborne = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(60), |readiness| {
        readiness.heartbeat_alive && readiness.armed
    })
    .await
    .expect("readiness after TAKEOFF");
    let airborne_readiness = airborne.readiness.as_ref().expect("readiness present after TAKEOFF");
    assert!(airborne_readiness.armed, "TAKEOFF checkpoint should be armed");

    if capture_tlog_fixtures_enabled() {
        let set_mode_tlog_start = worker.mavlink_tlog_len();
        let _set_mode = run_flight_command(
            &nats_client,
            &mut result_server.rx,
            runtime.clone(),
            manifest.clone(),
            serde_json::json!({ "command": "set_mode", "mode": "OFFBOARD" }),
        )
        .await;
        capture_compliance_tlog(&mut fixture_capture, &worker, "set_mode", set_mode_tlog_start).await;

        let goto_tlog_start = worker.mavlink_tlog_len();
        let _goto = run_flight_command(
            &nats_client,
            &mut result_server.rx,
            runtime.clone(),
            manifest.clone(),
            serde_json::json!({
                "command": "goto",
                "latitude_deg": 47.3977,
                "longitude_deg": 8.5456,
                "relative_altitude_m": 50.0
            }),
        )
        .await;
        capture_compliance_tlog(
            &mut fixture_capture,
            &worker,
            "goto_global_relative_alt_int",
            goto_tlog_start,
        )
        .await;
    }

    tokio::time::sleep(Duration::from_secs(10)).await;

    let rtl_tlog_start = worker.mavlink_tlog_len();
    let rtl = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime.clone(),
        manifest.clone(),
        serde_json::json!({ "command": "rtl" }),
    )
    .await;
    capture_compliance_tlog(&mut fixture_capture, &worker, "rtl", rtl_tlog_start).await;
    assert_task_result_accepted("rtl", &rtl);

    let land_tlog_start = worker.mavlink_tlog_len();
    let land = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime,
        manifest,
        serde_json::json!({ "command": "land", "altitude_m": 0.0 }),
    )
    .await;
    capture_compliance_tlog(&mut fixture_capture, &worker, "land", land_tlog_start).await;
    assert_task_result_accepted("land", &land);

    let landed = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(60), |readiness| {
        readiness.heartbeat_alive
    })
    .await
    .expect("readiness after LAND");
    let landed_readiness = landed.readiness.as_ref().expect("readiness present after LAND");
    assert!(
        landed_readiness.heartbeat_alive,
        "LAND checkpoint should keep heartbeat alive: {landed_readiness:?}"
    );

    let disarm_tlog_start = worker.mavlink_tlog_len();
    let disarm = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        drone_runtime(),
        empty_control_manifest(),
        serde_json::json!({ "command": "disarm" }),
    )
    .await;
    capture_compliance_tlog(&mut fixture_capture, &worker, "disarm", disarm_tlog_start).await;
    assert_task_result_accepted("disarm", &disarm);

    let ready_tlog_start = worker.mavlink_tlog_len();
    let post_disarm = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(60), |readiness| {
        readiness.ready_to_arm
    })
    .await
    .expect("readiness after DISARM");
    let post_disarm_readiness = post_disarm.readiness.as_ref().expect("readiness present after DISARM");
    assert!(
        post_disarm_readiness.ready_to_arm,
        "DISARM checkpoint should return to ready-to-arm posture: {post_disarm_readiness:?}"
    );
    capture_readiness_tlog(&mut fixture_capture, &worker, "ready", ready_tlog_start).await;

    fixture_capture.assert_clean();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "opt-in direct native-MAVLink QGC coexistence diagnostic; set ROZ_RUN_NATIVE_PX4_QGC_E2E=1"]
async fn qgc_coexistence_during_takeoff() {
    if std::env::var("PX4_SITL_DISABLE").is_ok() {
        eprintln!("SKIP: PX4_SITL_DISABLE set");
        return;
    }
    if std::env::var("ROZ_RUN_NATIVE_PX4_QGC_E2E").is_err() {
        eprintln!(
            "SKIP: direct native-MAVLink QGC coexistence diagnostic not requested. The default Substrate Docker \
             acceptance remains bridge-backed and does not require a host-side GCS peer."
        );
        return;
    }

    let worker_bin = cargo_bin("roz-worker");
    if !worker_bin.exists() {
        eprintln!(
            "SKIP: {} does not exist; build it first with `cargo build -p roz-worker --features test-fixtures`",
            worker_bin.display()
        );
        return;
    }

    let Some(mavlink_transport) = direct_mavlink_transport_or_skip() else {
        return;
    };
    let Some(gcs_udp_port) = direct_gcs_port_or_skip() else {
        return;
    };
    let Some(nats) = start_nats_or_skip().await else {
        return;
    };

    let mut result_server = spawn_result_server().await.expect("result callback server");
    let nats_client = async_nats::connect(nats.url()).await.expect("connect NATS");
    let telemetry_subject = Subjects::telemetry_state(TEST_WORKER_ID).expect("telemetry subject");
    let mut telemetry_sub = nats_client
        .subscribe(telemetry_subject)
        .await
        .expect("subscribe telemetry");

    let worker = spawn_worker(&worker_bin, nats.url(), &result_server.url, &mavlink_transport)
        .await
        .expect("spawn worker subprocess");

    let boot = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(120), |readiness| {
        readiness.heartbeat_alive
    })
    .await
    .expect("worker should publish MAVLink readiness telemetry");
    assert!(
        boot.readiness
            .as_ref()
            .is_some_and(|readiness| readiness.heartbeat_alive),
        "PX4 heartbeat must be alive before QGC shim starts"
    );

    let shim = roz_test::spawn_qgc_shim(gcs_udp_port, None);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let runtime = drone_runtime();
    let manifest = empty_control_manifest();
    let arm = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime.clone(),
        manifest.clone(),
        serde_json::json!({ "command": "arm" }),
    )
    .await;
    assert_task_result_accepted("arm", &arm);

    let takeoff = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime.clone(),
        manifest.clone(),
        serde_json::json!({ "command": "takeoff", "altitude_m": TAKEOFF_ALTITUDE_M }),
    )
    .await;
    assert_task_result_accepted("takeoff", &takeoff);

    let airborne = wait_for_readiness_matching(&mut telemetry_sub, Duration::from_secs(60), |readiness| {
        readiness.heartbeat_alive && readiness.armed
    })
    .await
    .expect("readiness after TAKEOFF with QGC shim active");
    assert!(
        airborne.readiness.as_ref().is_some_and(|readiness| readiness.armed),
        "TAKEOFF checkpoint should be armed with QGC shim active"
    );

    let worker_logs = worker.logs().to_ascii_lowercase();
    assert!(
        !worker_logs.contains("duplicate sequence"),
        "QGC coexistence diagnostic saw duplicate sequence warning in worker logs"
    );
    assert!(
        !worker_logs.contains("link conflict"),
        "QGC coexistence diagnostic saw link conflict warning in worker logs"
    );

    let land = run_flight_command(
        &nats_client,
        &mut result_server.rx,
        runtime,
        manifest,
        serde_json::json!({ "command": "land", "altitude_m": 0.0 }),
    )
    .await;
    assert_task_result_accepted("land", &land);

    shim.stop();
    drop(worker);
    drop(result_server);
    drop(nats_client);
    drop(nats);

    std::process::exit(0);
}

fn assert_task_result_accepted(command: &str, result: &TaskResult) {
    assert_eq!(
        result.status.as_str(),
        "succeeded",
        "{command} task should succeed: {result:?}"
    );
    let output = result
        .output
        .as_ref()
        .map_or_else(String::new, serde_json::Value::to_string);
    assert!(
        output.contains("\"mav_result\":\"accepted\""),
        "{command} task result should contain accepted MAVLink ACK; output={output}"
    );
}

fn capture_tlog_fixtures_enabled() -> bool {
    std::env::var("ROZ_PX4_CAPTURE_TLOG_FIXTURES").as_deref() == Ok("1")
}

async fn capture_compliance_tlog(
    report: &mut FixtureCaptureReport,
    worker: &WorkerProcess,
    verb: &str,
    start_offset: u64,
) {
    if !capture_tlog_fixtures_enabled() {
        return;
    }
    let captured = worker.mavlink_tlog_since(start_offset).await;
    report.record(write_or_diff_tlog(&format!("compliance/px4/{verb}.tlog"), &captured).await);
}

async fn capture_readiness_tlog(
    report: &mut FixtureCaptureReport,
    worker: &WorkerProcess,
    state: &str,
    start_offset: u64,
) {
    if !capture_tlog_fixtures_enabled() {
        return;
    }
    let captured = worker.mavlink_tlog_since(start_offset).await;
    report.record(write_or_diff_tlog(&format!("readiness/px4/{state}.tlog"), &captured).await);
}

async fn write_or_diff_tlog(rel_path: &str, captured: &[u8]) -> FixtureCaptureOutcome {
    if !capture_tlog_fixtures_enabled() {
        return FixtureCaptureOutcome::Disabled;
    }
    assert!(
        !captured.is_empty(),
        "FIXTURE CAPTURE EMPTY for {rel_path}: MAVLink recorder produced no bytes"
    );
    let rel_path = rel_path.to_string();
    let path = fixtures_root().join(&rel_path);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("create fixture parent");
    }

    if path.exists() {
        let committed = tokio::fs::read(&path).await.expect("read committed fixture");
        if committed != captured {
            eprintln!(
                "FIXTURE DIFF on {rel_path}: committed bytes differ from direct-endpoint capture. D-16 verify-only \
                 mode is engaged; inspect the new capture before accepting fixture drift."
            );
            return FixtureCaptureOutcome::Diff {
                rel_path,
                committed_size: committed.len(),
                captured_size: captured.len(),
            };
        }
        eprintln!("FIXTURE OK: {rel_path} ({} bytes)", captured.len());
        FixtureCaptureOutcome::Matched
    } else {
        tokio::fs::write(&path, captured)
            .await
            .expect("write fixture bootstrap");
        eprintln!(
            "FIXTURE BOOTSTRAP: wrote {rel_path} ({} bytes). Inspect the captured fixture, commit it with \
             `git add -f crates/roz-mavlink/tests/fixtures/{rel_path}`, then rerun in verify-only mode.",
            captured.len()
        );
        FixtureCaptureOutcome::Bootstrapped {
            rel_path,
            size: captured.len(),
        }
    }
}

fn fixtures_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("roz-test manifest dir should be under crates/")
        .join("roz-mavlink")
        .join("tests")
        .join("fixtures")
}

async fn run_flight_command(
    nats: &async_nats::Client,
    result_rx: &mut tokio::sync::mpsc::Receiver<TaskResult>,
    runtime: EmbodimentRuntime,
    control_manifest: ControlInterfaceManifest,
    command: serde_json::Value,
) -> TaskResult {
    let task_id = Uuid::new_v4();
    let mut status_sub = nats
        .subscribe(roz_nats::dispatch::task_status_subject(task_id))
        .await
        .expect("subscribe task status");

    let mut invocation = TaskInvocation::new(
        task_id,
        TEST_TENANT_ID.to_string(),
        format!("Use flight_command with this exact JSON input: {command}"),
        Uuid::new_v4(),
        None,
        Uuid::new_v4(),
        90,
        ExecutionMode::OodaReAct,
        None,
        "http://127.0.0.1:0".to_string(),
        None,
        Vec::new(),
        Some(control_manifest),
        None,
        None,
        None,
    );
    invocation.embodiment_runtime = Some(runtime);

    let subject = Subjects::invoke(TEST_WORKER_ID, &task_id.to_string()).expect("invoke subject");
    nats.publish(
        subject,
        serde_json::to_vec(&invocation)
            .expect("serialize TaskInvocation")
            .into(),
    )
    .await
    .expect("publish invocation");
    nats.flush().await.expect("flush invocation");

    let terminal = tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(message) = status_sub.next().await {
            let event: TaskStatusEvent = serde_json::from_slice(&message.payload).expect("decode TaskStatusEvent");
            if matches!(
                event.status.as_str(),
                "succeeded" | "failed" | "timed_out" | "cancelled" | "safety_stop"
            ) {
                return event;
            }
        }
        panic!("task status subscription closed before terminal status");
    })
    .await
    .expect("terminal task status");
    assert_eq!(
        terminal.status, "succeeded",
        "flight command task should succeed: {terminal:?}"
    );

    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(result) = result_rx.recv().await {
            if result.task_id == task_id {
                return result;
            }
        }
        panic!("result callback server closed");
    })
    .await
    .expect("task result callback")
}

async fn wait_for_readiness_matching(
    sub: &mut async_nats::Subscriber,
    timeout: Duration,
    predicate: fn(&ReadinessState) -> bool,
) -> anyhow::Result<TelemetryUpdate> {
    tokio::time::timeout(timeout, async {
        while let Some(message) = sub.next().await {
            let update = TelemetryUpdate::decode(message.payload.as_ref())?;
            if update.readiness.as_ref().is_some_and(predicate) {
                return Ok(update);
            }
        }
        anyhow::bail!("telemetry subscription closed");
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for readiness telemetry"))?
}

async fn spawn_worker(
    worker_bin: &PathBuf,
    nats_url: &str,
    restate_url: &str,
    mavlink_transport: &str,
) -> anyhow::Result<WorkerProcess> {
    let data_dir = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    let mavlink_tlog_path = std::env::var_os("ROZ_PX4_ARTIFACT_DIR").map_or_else(
        || data_dir.path().join("mavlink.tlog"),
        |dir| {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).expect("create ROZ_PX4_ARTIFACT_DIR");
            dir.join(format!("{}-{}-mavlink.tlog", TEST_WORKER_ID, std::process::id()))
        },
    );
    let mut cmd = Command::new(worker_bin);
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string()))
        .env("LOGFIRE_SEND_TO_LOGFIRE", "no")
        .env("ROZ_WORKER_ID", TEST_WORKER_ID)
        .env("ROZ_API_URL", "http://127.0.0.1:0")
        .env("ROZ_API_KEY", "")
        .env("ROZ_NATS_URL", nats_url)
        .env("ROZ_RESTATE_URL", restate_url)
        .env("ROZ_GATEWAY_URL", "http://127.0.0.1:0")
        .env("ROZ_GATEWAY_API_KEY", "test")
        .env("ROZ_MODEL_NAME", "test-flight-command")
        .env("ROZ_MODEL_TIMEOUT_SECS", "10")
        .env("ROZ_MAX_CONCURRENT_TASKS", "1")
        .env("ROZ_DATA_DIR", data_dir.path())
        .env("ROZ_CAMERA__ENABLED", "false")
        .env("ROZ_OBSERVABILITY__CAMERA__RECORD", "off")
        .env("ROZ_MAVLINK__TRANSPORT", mavlink_transport)
        .env("ROZ_MAVLINK__AUTOPILOT_HINT", "px4")
        .env("ROZ_MAVLINK__SIGNING__POSTURE", "off")
        .env("ROZ_MAVLINK_TLOG_PATH", &mavlink_tlog_path)
        .env("RUST_LOG", "info,roz_worker=debug,roz_mavlink=debug")
        .current_dir(cwd.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let logs = pipe_child_logs(TEST_WORKER_ID, &mut child);
    Ok(WorkerProcess {
        child,
        _data_dir: data_dir,
        _cwd: cwd,
        logs,
        mavlink_tlog_path,
    })
}

fn direct_mavlink_transport_or_skip() -> Option<String> {
    if let Ok(url) = std::env::var("PX4_SITL_MAVLINK_URL") {
        return Some(url);
    }
    if let Ok(port) = std::env::var("PX4_SITL_MAVLINK_PORT") {
        return Some(format!("udpin:0.0.0.0:{port}"));
    }

    eprintln!(
        "SKIP: direct native-MAVLink endpoint not configured. Set PX4_SITL_MAVLINK_URL \
         (or PX4_SITL_MAVLINK_PORT) to a real FCU/HITL/direct-SITL endpoint. The default \
         bedrockdynamics/substrate-sim:px4-gazebo-humble path is bridge-backed and is covered \
         by roz-local::env_start_px4_docker_wasm_velocity_flies_10m."
    );
    None
}

fn direct_gcs_port_or_skip() -> Option<u16> {
    let Ok(port) = std::env::var("PX4_SITL_GCS_PORT") else {
        eprintln!("SKIP: direct native-MAVLink QGC diagnostic requires PX4_SITL_GCS_PORT for the GCS peer.");
        return None;
    };
    Some(port.parse().expect("PX4_SITL_GCS_PORT must be a u16"))
}

async fn start_nats_or_skip() -> Option<roz_test::NatsGuard> {
    match tokio::time::timeout(Duration::from_secs(60), tokio::spawn(roz_test::nats_container())).await {
        Ok(Ok(guard)) => Some(guard),
        Ok(Err(error)) => {
            eprintln!("SKIP: NATS testcontainer failed to start: {error}");
            None
        }
        Err(_) => {
            eprintln!("SKIP: NATS testcontainer boot timeout (>60s)");
            None
        }
    }
}

fn pipe_child_logs(worker_id: &str, child: &mut Child) -> Arc<Mutex<String>> {
    let logs = Arc::new(Mutex::new(String::new()));
    if let Some(stdout) = child.stdout.take() {
        let worker = worker_id.to_string();
        let logs = Arc::clone(&logs);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        eprint!("[{worker} stdout] {line}");
                        logs.lock().expect("worker log buffer poisoned").push_str(&line);
                    }
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let worker = worker_id.to_string();
        let logs = Arc::clone(&logs);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        eprint!("[{worker} stderr] {line}");
                        logs.lock().expect("worker log buffer poisoned").push_str(&line);
                    }
                }
            }
        });
    }
    logs
}

async fn spawn_result_server() -> anyhow::Result<ResultServer> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = handle_result_connection(stream, tx).await;
            });
        }
    });
    Ok(ResultServer { url, rx, handle })
}

async fn handle_result_connection(
    mut stream: tokio::net::TcpStream,
    tx: tokio::sync::mpsc::Sender<TaskResult>,
) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers");
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(idx) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break idx + 4;
        }
    };

    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    while bytes.len() < header_end + content_length {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("connection closed before body");
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    let body = &bytes[header_end..header_end + content_length];
    if !body.is_empty()
        && let Ok(result) = serde_json::from_slice::<TaskResult>(body)
    {
        let _ = tx.send(result).await;
    }
    stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
        .await?;
    Ok(())
}

fn drone_runtime() -> EmbodimentRuntime {
    let mut frame_tree = FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);
    frame_tree
        .add_frame("base_link", "world", Transform3D::identity(), FrameSource::Static)
        .expect("world root exists");

    let mut model = EmbodimentModel {
        model_id: "phase27-px4-sitl-drone".to_string(),
        model_digest: String::new(),
        embodiment_family: Some(EmbodimentFamily {
            family_id: "drone-mavlink".to_string(),
            description: "PX4 SITL drone reached through the native MAVLink backend".to_string(),
        }),
        links: vec![
            Link {
                name: "world".to_string(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            },
            Link {
                name: "base_link".to_string(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            },
        ],
        joints: Vec::new(),
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames: vec!["world".to_string(), "base_link".to_string()],
        channel_bindings: Vec::new(),
    };
    model.stamp_digest();
    EmbodimentRuntime::compile(model, None, None)
}

fn empty_control_manifest() -> ControlInterfaceManifest {
    let mut manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: Vec::new(),
        bindings: Vec::new(),
    };
    manifest.stamp_digest();
    manifest
}

fn cargo_bin(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    path.push("debug");
    path.push(name);
    path
}
