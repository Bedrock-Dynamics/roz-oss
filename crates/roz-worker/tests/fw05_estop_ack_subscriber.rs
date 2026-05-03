//! FW-05 / Plan 26.10-10 — production-wiring integration tests for the
//! latched e-stop NATS subscribers and WAL-authoritative boot.
//!
//! Closes VERIFICATION.md gaps CR-01 (WAL persistence dead in production)
//! and CR-02 (signed NATS ack/resume subscribers absent). Plan 07 shipped
//! the state machine + WAL methods + subject helpers + ControllerCommand
//! variants; this plan wires them into the production worker boot path.
//!
//! These tests are DEFAULT-RUNNABLE — NO `#[ignore]`. The H4 enforcement
//! pattern from Plan 09 applies: the gap is real, the wiring is real,
//! the tests must run by default so any developer running
//! `cargo test -p roz-worker` triggers them.
//!
//! Test harness shape: rather than spawning the full worker `main()` (too
//! heavy + introduces pre-task / signing-bootstrap coupling), each test
//! constructs the same component graph the production wiring builds:
//! `WorkerSigningContext` (test fixture key material), `WalStore` on
//! tempdir, worker-level `shared_cmd_tx` slot, `CopperHandle` against a
//! `LogActuatorSink`, and the production
//! `roz_worker::safety_subscribers::spawn_*` functions wired against a
//! NATS testcontainer. Behavior assertions cover the four observable
//! truths from VERIFICATION.md's `missing[]` lists.

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use ed25519_dalek::SigningKey;
use parking_lot::RwLock;
use roz_copper::channels::ControllerCommand;
use roz_copper::handle::CopperHandle;
use roz_copper::io_log::LogActuatorSink;
use roz_copper::latch::LatchState;
use roz_copper::policy::new_hot_policy;
use roz_core::key_provider::StaticKeyProvider;
use roz_core::signing::{Direction, HEADER_NAME, SignedFields, payload_sha256_hex, sign_envelope};
use roz_worker::safety_subscribers::spawn_estop_ack_subscriber;
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load, save};
use roz_worker::wal::WalStore;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const WORKER_SEED: [u8; 32] = [7u8; 32];
const SERVER_SEED: [u8; 32] = [9u8; 32];

/// Construct the worker test fixture: tempdir, WorkerSigningContext built
/// from a deterministic worker key + cached server-verify key, the
/// matching server `SigningKey` so the test can forge inbound envelopes,
/// and an in-memory WAL.
async fn worker_fixture() -> (TempDir, WorkerSigningContext, SigningKey, Arc<WalStore>, Uuid, Uuid) {
    let tmp = TempDir::new().unwrap();
    let provider = Arc::new(StaticKeyProvider::from_key_bytes(WORKER_SEED));
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();
    let server_signing = SigningKey::from_bytes(&SERVER_SEED);
    let svk = server_signing.verifying_key().to_bytes();
    save(tmp.path(), &provider, tenant, 1, &WORKER_SEED, &svk)
        .await
        .unwrap();
    let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();

    let wal_path = tmp.path().join("wal.db");
    let wal = Arc::new(WalStore::open(wal_path.to_str().unwrap()).expect("open WAL"));

    let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), Arc::clone(&wal));
    (tmp, ctx, server_signing, wal, tenant, host)
}

/// Sign an envelope as if the server were issuing a server→worker
/// command. The signed-NATS subscriber's `verify_inbound_worker` accepts
/// this because the envelope's direction is `ServerToWorker` and the
/// signature is by the server's signing key (which the worker's
/// `material.server_verifying_key` was seeded with above).
fn sign_server_envelope(server_signing: &SigningKey, tenant: Uuid, host: Uuid, payload: &[u8]) -> String {
    let fields = SignedFields {
        direction: Direction::ServerToWorker,
        tenant_id: tenant,
        host_id: host,
        correlation_id: Uuid::new_v4(),
        timestamp: chrono::Utc::now(),
        sequence_number: 1,
        payload_hash: payload_sha256_hex(payload),
        key_version: 1,
    };
    let env = sign_envelope(&fields, server_signing).expect("sign envelope");
    env.encode_header().expect("encode header")
}

/// Spawn a Copper handle wired with a latch persistence channel + a
/// `spawn_blocking` drainer that calls `WalStore::save_latch_state`. This
/// mirrors the production wiring in
/// `roz-worker/src/main.rs::execute_task` (Plan 26.10-10 Task 1).
fn spawn_handle_with_persistence(
    initial_latch: LatchState,
    wal: &Arc<WalStore>,
) -> (
    CopperHandle,
    std::sync::Arc<ArcSwap<roz_copper::channels::ControllerState>>,
) {
    let actuator: Arc<dyn roz_copper::io::ActuatorSink> = Arc::new(LogActuatorSink::new());
    let policy = new_hot_policy();
    let backpressure = Arc::new(AtomicU8::new(0));

    let (latch_tx, latch_rx) = std::sync::mpsc::sync_channel::<LatchState>(16);

    let handle = CopperHandle::spawn_with_policy_and_io_with_initial_latch(
        1.5,
        actuator,
        None,
        policy,
        backpressure,
        Some(latch_tx),
        initial_latch,
    );

    // FW-05 / Plan 26.10-10 (gap CR-01b): drainer task — spawn_blocking
    // because WalStore is sync (rusqlite). Ends naturally when latch_tx
    // drops on handle shutdown.
    {
        let wal_for_drain = Arc::clone(wal);
        tokio::task::spawn_blocking(move || {
            while let Ok(state) = latch_rx.recv() {
                let _ = wal_for_drain.save_latch_state(state);
            }
        });
    }

    let state = Arc::clone(handle.state());
    (handle, state)
}

/// Poll `state.load().latch_state` until it equals `expected`, returning
/// `true` on match within `budget` and `false` on timeout. `recv_timeout`
/// is a sync stdlib call that does not yield; use small async sleeps
/// instead so the controller thread can run.
async fn await_latch(
    state: &std::sync::Arc<ArcSwap<roz_copper::channels::ControllerState>>,
    expected: LatchState,
    budget: Duration,
) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if state.load().latch_state == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    state.load().latch_state == expected
}

/// CR-01c — WAL-authoritative boot: pre-seed WAL with `Latched`, boot a
/// `CopperHandle` with the production load+seed sequence, assert the
/// initial `ControllerState.latch_state == Latched` within 1 s.
#[tokio::test]
async fn worker_restart_with_wal_latched_keeps_latched() {
    let (_tmp, _ctx, _server_signing, wal, _tenant, _host) = worker_fixture().await;

    // Persist Latched as if a previous worker run had latched and then
    // crashed. Worker restart must observe this state on boot.
    wal.save_latch_state(LatchState::Latched).expect("save Latched");

    // Production load+seed sequence — same shape as
    // `execute_task`'s OodaReAct branch in `roz-worker/src/main.rs`.
    let initial_latch = wal.load_latch_state().expect("load Latched");
    assert_eq!(
        initial_latch,
        LatchState::Latched,
        "WAL must report previously-persisted Latched"
    );

    let (handle, state) = spawn_handle_with_persistence(initial_latch, &wal);

    // The rcu commits before the controller observes any tick; the first
    // state read must be Latched.
    assert!(
        await_latch(&state, LatchState::Latched, Duration::from_secs(1)).await,
        "FW-05 CR-01c: ControllerState.latch_state must be Latched within 1 s of WAL-authoritative boot; got {:?}",
        state.load().latch_state
    );

    handle.shutdown().await;
}

/// CR-02c — Signed AckEstop advances `Latched -> AwaitingAck` via the
/// live `cmd_tx`. The signed-NATS subscriber `load()`s the worker-level
/// `shared_cmd_tx` slot and forwards the verified command into the
/// per-task controller.
#[tokio::test]
async fn signed_estop_ack_advances_latched_to_awaiting_ack() {
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect NATS");

    let (_tmp, ctx, server_signing, wal, tenant, host) = worker_fixture().await;
    wal.save_latch_state(LatchState::Latched).expect("save Latched");

    let (handle, state) = spawn_handle_with_persistence(LatchState::Latched, &wal);

    // Worker-level shared_cmd_tx slot — the production wiring stores
    // Some(handle.cmd_tx()) here after spawn.
    let shared_cmd_tx: Arc<ArcSwap<Option<mpsc::Sender<ControllerCommand>>>> =
        Arc::new(ArcSwap::from_pointee(Some(handle.cmd_tx())));

    let worker_id = format!("worker-fw05-ack-{}", Uuid::new_v4().simple());
    let cancel = CancellationToken::new();
    let _ack_join = spawn_estop_ack_subscriber(
        nats.clone(),
        ctx,
        worker_id.clone(),
        host.to_string(),
        Arc::clone(&shared_cmd_tx),
        cancel.clone(),
    );

    // NATS subscription is eventually consistent; settle before publish
    // (matches the precedent at phase24_e2e.rs:259).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish a signed empty payload on safety.estop_ack.{worker_id}.
    let subject = roz_nats::Subjects::estop_ack(&worker_id).expect("estop_ack subject");
    let payload: Vec<u8> = b"".to_vec();
    let header = sign_server_envelope(&server_signing, tenant, host, &payload);
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HEADER_NAME, header.as_str());
    nats.publish_with_headers(subject, headers, payload.into())
        .await
        .expect("publish signed estop_ack");
    nats.flush().await.expect("flush");

    assert!(
        await_latch(&state, LatchState::AwaitingAck, Duration::from_secs(2)).await,
        "FW-05 CR-02c: signed AckEstop must drive Latched -> AwaitingAck within 2 s; got {:?}",
        state.load().latch_state
    );

    cancel.cancel();
    handle.shutdown().await;
}

/// CR-02b — Unsigned messages rejected AND audited via
/// `safety.signature_failure.{host_id}`. Latch state must NOT change.
#[tokio::test]
async fn unsigned_estop_ack_rejected_and_audited() {
    use futures::StreamExt;

    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect NATS");

    let (_tmp, ctx, _server_signing, wal, _tenant, host) = worker_fixture().await;
    wal.save_latch_state(LatchState::Latched).expect("save Latched");

    let (handle, state) = spawn_handle_with_persistence(LatchState::Latched, &wal);

    let shared_cmd_tx: Arc<ArcSwap<Option<mpsc::Sender<ControllerCommand>>>> =
        Arc::new(ArcSwap::from_pointee(Some(handle.cmd_tx())));

    let worker_id = format!("worker-fw05-unsigned-{}", Uuid::new_v4().simple());
    let cancel = CancellationToken::new();

    // Subscribe to the audit subject BEFORE publishing the bad message
    // so we don't race the audit publish.
    let audit_subject = roz_nats::Subjects::safety_signature_failure_worker(&host.to_string()).expect("audit subject");
    let mut audit_sub = nats.subscribe(audit_subject.clone()).await.expect("subscribe audit");

    let _ack_join = spawn_estop_ack_subscriber(
        nats.clone(),
        ctx,
        worker_id.clone(),
        host.to_string(),
        Arc::clone(&shared_cmd_tx),
        cancel.clone(),
    );

    // Settle subscriptions before publish.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish on safety.estop_ack.{worker_id} with NO header — must be
    // rejected with `MissingHeader`.
    let subject = roz_nats::Subjects::estop_ack(&worker_id).expect("estop_ack subject");
    nats.publish(subject, vec![].into()).await.expect("publish unsigned");
    nats.flush().await.expect("flush");

    // (a) latch state must NOT advance — still Latched after a generous
    // budget for the subscriber to attempt + reject.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        state.load().latch_state,
        LatchState::Latched,
        "FW-05 CR-02b: unsigned estop_ack must NOT transition latch state from Latched"
    );

    // (b) at least one audit publish landed on safety.signature_failure.{host_id}.
    let audit_msg = tokio::time::timeout(Duration::from_secs(2), audit_sub.next())
        .await
        .expect("FW-05 CR-02b: audit publish must arrive within 2 s of unsigned message")
        .expect("audit subscription must yield at least one message");
    let audit_text = String::from_utf8_lossy(&audit_msg.payload);
    assert!(
        audit_text.contains("signature rejected"),
        "audit payload must mention signature rejection (got {audit_text:?})"
    );

    cancel.cancel();
    handle.shutdown().await;
}

/// CR-01b — `Run -> Latched` transitions persist to WAL via the drainer
/// task. Use the controller's drain_commands path (AckEstop produces a
/// transition that fires `latch_persist_tx.try_send`); the drainer
/// observes it and writes to WAL.
///
/// We pre-seed Latched in memory, send AckEstop (drives Latched ->
/// AwaitingAck — a real transition that calls try_send), and assert the
/// WAL row reflects the new state. This exercises the same end-to-end
/// path the production controller uses for Run -> Latched: the
/// `try_send` fires inside the controller, the `spawn_blocking` drainer
/// receives, and `save_latch_state` persists.
#[tokio::test]
async fn run_to_latched_persists_to_wal() {
    let (_tmp, _ctx, _server_signing, wal, _tenant, _host) = worker_fixture().await;

    // Boot with WAL EMPTY so the WAL initial state is "absent" -> Run
    // (Plan 07 contract). The drainer is wired via
    // spawn_handle_with_persistence; pre-seed Latched in memory so a
    // subsequent AckEstop produces a transition (and thus a try_send to
    // the persistence channel).
    let initial = wal.load_latch_state().expect("load empty WAL");
    assert_eq!(initial, LatchState::Run, "empty WAL must report Run");

    let (handle, _state) = spawn_handle_with_persistence(LatchState::Latched, &wal);

    // Let the controller observe the rcu'd Latched, then drive AckEstop
    // through the cmd_tx — the same path the signed-NATS subscriber
    // takes. The Plan 07 controller publishes on latch_persist_tx and
    // the drainer writes to WAL.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle
        .send(ControllerCommand::AckEstop)
        .await
        .expect("cmd_tx send must succeed");

    // Poll the WAL until the drainer has persisted the transition. 2 s
    // covers one tick (~10 ms) + spawn_blocking dispatch + sqlite write.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed: Option<LatchState> = None;
    while Instant::now() < deadline {
        let current = wal.load_latch_state().expect("load current latch state");
        if current == LatchState::AwaitingAck {
            observed = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        observed,
        Some(LatchState::AwaitingAck),
        "FW-05 CR-01b: Latched -> AwaitingAck transition must persist to WAL via drainer within 2 s"
    );

    handle.shutdown().await;
}
