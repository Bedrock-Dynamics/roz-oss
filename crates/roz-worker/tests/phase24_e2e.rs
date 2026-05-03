//! Phase 24 end-to-end integration scenarios.
//!
//! - `phase24_deadman_survives_nats_outage` — deterministic worker-local
//!   watchdog test proving FS-01's "edge-local deadman" invariant: the
//!   watchdog keeps running on a local pet loop even with zero NATS
//!   activity, so a network outage does NOT trip the deadman.
//! - `phase24_resume_path_with_fresh_checkpoint` — deterministic end-to-end
//!   resume-gate scenario wired against a real `WalStore`. Persists a
//!   checkpoint, reads its age back via `checkpoint_age_secs`, and asserts
//!   `decide_recovery` returns `ResumeFromCheckpoint`.
//! - `phase24_induced_30s_nats_outage_survives_buffering_and_replay` —
//!   `#[ignore]`-gated hero scenario requiring Docker for a NATS +
//!   toxiproxy testcontainer pair. Exercises the full SC#3 / SC#4 loop:
//!   normal publishes land on NATS → toxiproxy-induced outage forces
//!   `publish_state_signed_with_buffer` into its WAL-fallback branch →
//!   `TelemetryReplay::run_once` drains the WAL after the proxy comes
//!   back → server-side dedup accepts every replayed seq exactly once.

use roz_core::edge::recovery::{CrashState, RecoveryStrategy};
use roz_worker::recovery::decide_recovery;
use roz_worker::wal::WalStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// SC#2: deadman does NOT trip during a simulated NATS outage because the
/// watchdog is edge-local. This test proves the watchdog is NOT broker-
/// dependent — a healthy local pet path keeps it alive independent of
/// NATS availability.
#[tokio::test]
async fn phase24_deadman_survives_nats_outage() {
    use roz_worker::command_watchdog::{CommandWatchdog, OnExpireCallback};

    let fired = Arc::new(AtomicBool::new(false));
    let fired_clone = fired.clone();
    let cb: OnExpireCallback = Arc::new(move || {
        fired_clone.store(true, Ordering::Relaxed);
    });
    let wd = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_secs(2), // deadline
        cb,
    ));

    // Simulate 5 s of "NATS down" while the local control path keeps the
    // watchdog alive via pet. No NATS message ever arrives, yet the
    // deadman stays un-expired.
    let wd_task = wd.clone();
    let pet_task = tokio::spawn(async move {
        for _ in 0..50 {
            wd_task.pet();
            tokio::time::sleep(Duration::from_millis(100)).await; // 10 Hz pet
        }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    pet_task.abort();

    assert!(
        !fired.load(Ordering::Relaxed),
        "watchdog incorrectly fired during healthy-pet window"
    );
    assert!(
        !wd.is_latched(),
        "watchdog must not be latched while pet path is healthy"
    );
}

/// SC#5: end-to-end resume-gate scenario. Writes a fresh checkpoint to a
/// real `WalStore`, reads its age back, constructs a `CrashState` that
/// satisfies every D-11 predicate, and asserts `decide_recovery` returns
/// `ResumeFromCheckpoint`.
#[tokio::test]
async fn phase24_resume_path_with_fresh_checkpoint() {
    let wal = WalStore::open(":memory:").expect("open WAL");
    let ck = wal
        .append_checkpoint("task-e2e", 3, b"snapshot")
        .expect("append checkpoint");
    let age = wal
        .checkpoint_age_secs("task-e2e")
        .expect("checkpoint age query")
        .expect("checkpoint present");
    let now = chrono::Utc::now().timestamp();
    let state = CrashState {
        joint_positions: Some(vec![0.0, 1.0]),
        brakes_engaged: true,
        mid_action: true,
        task_id: Some("task-e2e".into()),
        last_wal_seq: Some(1),
        last_checkpoint_id: Some(ck.clone()),
        last_checkpoint_ts_unix: Some(now - age),
    };
    let decision = decide_recovery(&state, now);
    assert_eq!(
        decision.strategy,
        RecoveryStrategy::ResumeFromCheckpoint,
        "fresh checkpoint must resume"
    );
}

// ============================================================================
// SC#3 / SC#4: induced NATS outage → WAL buffer → replay → dedup round-trip
// ============================================================================
//
// Networking note (copied verbatim from the working pattern in
// `crates/roz-zenoh/tests/chaos_partition_recovery.rs` §"Topology"):
//
//   Both the NATS container and the toxiproxy container run under the host
//   Docker daemon. The NATS container publishes `4222` via a host-mapped
//   port (`nats_container()` reports `nats://localhost:{mapped_port}`). The
//   toxiproxy container cannot reach `localhost:{mapped_port}` because
//   `localhost` inside the toxiproxy container is the toxiproxy container
//   itself; we instead use `host.docker.internal:{mapped_port}` for the
//   upstream. Workers connect to
//   `nats://{toxi.host()}:{toxi.proxy_listener_host_port}`, which the
//   toxiproxy container forwards to `host.docker.internal:{mapped_port}`,
//   which Docker Desktop routes back to the host's `localhost:{mapped_port}`,
//   which the NATS container exposes as 4222 inside its own namespace.
//
// This is the same tradeoff documented in 16-RESEARCH §1: fine on Docker
// Desktop, may need `--network=host` on vanilla Linux CI.
//
// Outage mechanism (from advisor reconcile with the `async_nats` source):
//
//   `Client::publish_with_headers` only errors on `MaxPayloadExceeded` or
//   `Send` (internal command mpsc closed). The mpsc closes when the
//   connector task exits, which requires `ConnectOptions::max_reconnects`
//   to be finite AND the reconnect budget to be exhausted. We use
//   `max_reconnects(1)` + `proxy.disable()` to force the connector to
//   exceed its budget, at which point `flush()` returns an error and all
//   subsequent publishes return `Err(Send)`. That is the EXACT branch
//   `publish_state_signed_with_buffer` routes into its WAL-fallback path
//   at telemetry.rs:222. `async-nats` 0.38.0's `ConnectOptions` treats
//   `Some(0)` as "unlimited" — `Some(1)` is the smallest usable value.
//
// Anti-tautology gate: `OUTAGE_ENABLED` toggles both `proxy.disable()` and
// the flush-wait loop together. Flipping it to `false` makes the Phase 2
// assertion `wal.list_unacked_telemetry().len() == 3` fail, because the
// via-proxy client never goes terminal and every publish lands on NATS.
// Verified locally before commit — see commit message for the observed
// inverted-run failure text.

const OUTAGE_ENABLED: bool = true;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker for NATS + toxiproxy testcontainers"]
#[expect(
    clippy::too_many_lines,
    reason = "end-to-end round-trip covering normal-path publish, outage fallback, replay, and dedup — splitting hurts readability"
)]
async fn phase24_induced_30s_nats_outage_survives_buffering_and_replay() {
    use ed25519_dalek::SigningKey;
    use futures::StreamExt;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use roz_core::signing::{HEADER_NAME, SignatureEnvelope};
    use roz_nats::subjects::Subjects;
    use roz_worker::signing_hooks::WorkerSigningContext;
    use roz_worker::signing_key::{load, save};
    use roz_worker::telemetry::{DropCounter, publish_state_signed_with_buffer};
    use roz_worker::telemetry_backpressure::TelemetryBackpressure;
    use roz_worker::telemetry_replay::TelemetryReplay;
    use uuid::Uuid;

    const WORKER_ID: &str = "worker-phase24-17-outage";
    const WORKER_SEED: [u8; 32] = [11u8; 32];
    const SERVER_SEED: [u8; 32] = [13u8; 32];

    // ---- Step 1: NATS + toxiproxy containers ------------------------------
    let nats_guard = roz_test::nats_container().await;

    // `nats_container()` returns `nats://{host}:{mapped_port}`. We need the
    // mapped port on its own so the toxiproxy upstream can reach it via
    // host.docker.internal.
    let nats_url = nats_guard.url();
    let nats_port = nats_url
        .rsplit_once(':')
        .map(|(_, p)| p)
        .expect("nats_url shape must be 'nats://host:port'");

    let toxi = roz_test::toxiproxy::toxiproxy_container().await;
    // Use `create_proxy` (not `populate`) per the 16-01 A2 decision: the
    // shopify/toxiproxy 2.12.0 `/populate` response shape does not match
    // noxious-client 1.0.4's decoder; `POST /proxies` (i.e. `create_proxy`)
    // is the lowest-common-denominator both servers agree on.
    let mut proxy = toxi
        .client
        .create_proxy(
            "nats-phase24-17",
            "0.0.0.0:8666",
            &format!("host.docker.internal:{nats_port}"),
        )
        .await
        .expect("create_proxy nats-phase24-17");

    // ---- Step 2: worker surfaces ------------------------------------------
    let tmp = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(StaticKeyProvider::from_key_bytes(WORKER_SEED));
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();
    let server_signing = SigningKey::from_bytes(&SERVER_SEED);
    let svk = server_signing.verifying_key().to_bytes();
    save(tmp.path(), &provider, tenant, 1, &WORKER_SEED, &svk)
        .await
        .unwrap();
    let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();
    let wal = Arc::new(WalStore::open(":memory:").unwrap());
    let signing_ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal.clone());
    let backpressure = TelemetryBackpressure::new();
    let drop_counter = DropCounter::new();
    let append_counter = AtomicU64::new(0);

    // Connect to NATS VIA toxiproxy with a finite reconnect budget. See the
    // module-level "Outage mechanism" note for why `max_reconnects(1)` is the
    // smallest usable value. Once `proxy.disable()` severs the connection
    // AND the single retry fails (the proxy is disabled), the connector
    // emits `ClientError::MaxReconnects` and the command mpsc is closed,
    // which makes subsequent publishes return `Err(Send)`.
    let proxied_url = format!("nats://{}:{}", toxi.host(), toxi.proxy_listener_host_port);
    let nats_via_proxy = async_nats::ConnectOptions::new()
        .max_reconnects(1)
        .connect(&proxied_url)
        .await
        .expect("connect via toxiproxy");

    // Direct NATS connection for the server-side subscriber — we do not
    // want the outage to blind the subscriber too. The subject is the same
    // one `publish_state_signed_with_buffer` targets.
    let nats_direct = async_nats::connect(nats_url).await.expect("connect direct NATS");
    let subject = Subjects::telemetry_state(WORKER_ID).expect("telemetry subject");
    let mut sub = nats_direct.subscribe(subject.clone()).await.expect("subscribe state");

    // ---- Step 3: spawn the server-side drainer ----------------------------
    //
    // Collects `(seq, payload)` tuples until the subscription is idle for
    // 2 seconds, then returns. 2s covers a worst-case replay-cadence + NATS
    // propagation (BASE_INTERVAL_MS = 10 ms per frame × 3 frames + jitter).
    let drain_handle: tokio::task::JoinHandle<Vec<(u64, Vec<u8>)>> = tokio::spawn(async move {
        let mut collected: Vec<(u64, Vec<u8>)> = Vec::new();
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(2), sub.next()).await {
            let hdr_str = msg
                .headers
                .as_ref()
                .and_then(|h| h.get(HEADER_NAME))
                .map(|v| v.to_string());
            let Some(hdr_str) = hdr_str else {
                continue;
            };
            let env = match SignatureEnvelope::decode_header(&hdr_str) {
                Ok(e) => e,
                Err(_) => continue,
            };
            collected.push((env.fields.sequence_number, msg.payload.to_vec()));
        }
        collected
    });

    // Settle subscription before first publish (NATS is eventually
    // consistent on subscribe — 500ms matches precedents in 15-09 /
    // 16-04 dual-publish tests).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- Step 4: normal-path publishes ------------------------------------
    for step in 1..=3u64 {
        let data = serde_json::json!({ "step": step, "phase": "normal" });
        publish_state_signed_with_buffer(
            &nats_via_proxy,
            &signing_ctx,
            WORKER_ID,
            Uuid::new_v4(),
            &data,
            &wal,
            &backpressure,
            &drop_counter,
            &append_counter,
        )
        .await
        .expect("normal-path publish");
    }
    nats_via_proxy.flush().await.expect("flush after normal-path publishes");

    let unacked_pre = wal.list_unacked_telemetry().expect("list unacked pre");
    assert!(
        unacked_pre.is_empty(),
        "normal-path publishes must not buffer (got {} unacked): proxy path + healthy NATS should use the fast branch",
        unacked_pre.len()
    );

    // ---- Step 5: outage --------------------------------------------------
    if OUTAGE_ENABLED {
        proxy.disable().await.expect("disable proxy");

        // Wait for the via-proxy client's internal task to exhaust its
        // single-retry budget and close the command mpsc. `flush()` is the
        // earliest-surfacing signal — it errors as soon as the connector
        // reports `MaxReconnects`. Poll with short timeouts so the test
        // fails fast (10s budget) if the client never goes terminal.
        let terminal_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut went_terminal = false;
        while tokio::time::Instant::now() < terminal_deadline {
            match tokio::time::timeout(Duration::from_millis(500), nats_via_proxy.flush()).await {
                Ok(Err(_)) | Err(_) => {
                    went_terminal = true;
                    break;
                }
                Ok(Ok(())) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        assert!(
            went_terminal,
            "nats client never went terminal after proxy.disable() — \
             max_reconnects(1) did not close the command mpsc within 10s; \
             the outage branch would not fire"
        );
    }

    for step in 4..=6u64 {
        let data = serde_json::json!({ "step": step, "phase": "outage" });
        publish_state_signed_with_buffer(
            &nats_via_proxy,
            &signing_ctx,
            WORKER_ID,
            Uuid::new_v4(),
            &data,
            &wal,
            &backpressure,
            &drop_counter,
            &append_counter,
        )
        .await
        .expect("outage-phase publish must still return Ok via the WAL-fallback path");
    }

    let unacked_mid = wal.list_unacked_telemetry().expect("list unacked mid");
    assert_eq!(
        unacked_mid.len(),
        3,
        "3 outage frames must land in WAL because the via-proxy client is terminal: got {}",
        unacked_mid.len()
    );

    // Brief assertion that the subscription received ONLY the 3 normal
    // frames during the outage window (nothing extra leaked through). We
    // don't consume the stream yet — leave it to the drainer. The structural
    // proof is: unacked_mid.len() == 3 and replay draws 3 frames out of
    // WAL and pushes them to NATS, which the drainer then observes as
    // "phase":"outage" — total 6.

    // ---- Step 6: restore --------------------------------------------------
    //
    // Remove the proxy toxic. The via-proxy client is terminal and cannot
    // be revived, so we use a fresh direct connection for replay.
    if OUTAGE_ENABLED {
        proxy.enable().await.expect("enable proxy");
    }

    let nats_replay = async_nats::connect(nats_url).await.expect("connect replay client");

    // ---- Step 7: replay --------------------------------------------------
    let replay = TelemetryReplay::new(wal.clone(), Arc::new(signing_ctx.clone()));
    let replayed = replay.run_once(&nats_replay, WORKER_ID).await.expect("replay run_once");
    assert_eq!(
        replayed, 3,
        "replay must drain exactly 3 buffered frames, got {replayed}"
    );
    nats_replay.flush().await.expect("flush after replay");

    let unacked_post = wal.list_unacked_telemetry().expect("list unacked post");
    assert!(
        unacked_post.is_empty(),
        "replay must ack all drained frames — got {} unacked after drain",
        unacked_post.len()
    );

    // ---- Step 8: dedup ---------------------------------------------------
    //
    // Drain the server subscription into `(seq, payload)` tuples and feed
    // every one through the real server-side dedup semantics (clone of
    // `roz_server::nats_handlers::check_telemetry_dedup` — see the
    // `phase24_outage_replay.rs` module doc for why we duplicate the
    // two-line helper instead of pulling roz-server into the integration-
    // test graph).
    let received = drain_handle.await.expect("drainer task");
    assert_eq!(
        received.len(),
        6,
        "server must see 3 normal + 3 replayed = 6 frames total (got {})",
        received.len()
    );

    // Sequence numbers must be strictly monotonically increasing — the
    // worker-side WAL signing counter never issues the same seq twice.
    for pair in received.windows(2) {
        assert!(
            pair[1].0 > pair[0].0,
            "sequence numbers must be strictly monotonic: {} -> {}",
            pair[0].0,
            pair[1].0
        );
    }

    // Phase tally — separately prove the outage-phase frames actually went
    // through the WAL-fallback + replay path (not silently re-delivered
    // via the healthy NATS connection after reconnect).
    let mut saw_normal = 0usize;
    let mut saw_outage = 0usize;
    for (_, payload) in &received {
        let v: serde_json::Value = serde_json::from_slice(payload).unwrap();
        match v["phase"].as_str() {
            Some("normal") => saw_normal += 1,
            Some("outage") => saw_outage += 1,
            other => panic!("unexpected payload phase: {other:?} full payload = {v:?}"),
        }
    }
    assert_eq!(saw_normal, 3, "expected 3 normal-phase frames, got {saw_normal}");
    assert_eq!(saw_outage, 3, "expected 3 outage-phase frames, got {saw_outage}");

    let dedup: std::sync::Mutex<std::collections::HashMap<String, u64>> = std::sync::Mutex::default();
    let check_dedup = |map: &std::sync::Mutex<std::collections::HashMap<String, u64>>, worker_id: &str, seq: u64| {
        let mut guard = map.lock().expect("dedup mutex poisoned");
        let entry = guard.entry(worker_id.to_string()).or_insert(0);
        if seq > *entry {
            *entry = seq;
            true
        } else {
            false
        }
    };

    let mut accepted = 0usize;
    for (seq, _) in &received {
        if check_dedup(&dedup, WORKER_ID, *seq) {
            accepted += 1;
        }
    }
    assert_eq!(
        accepted,
        received.len(),
        "every replayed frame carries a novel seq — dedup must accept all 6 in arrival order"
    );

    // Re-feed the highest seq — must be rejected.
    let highest_seq = received.iter().map(|(s, _)| *s).max().unwrap();
    assert!(
        !check_dedup(&dedup, WORKER_ID, highest_seq),
        "re-feeding the high-water seq ({highest_seq}) must be rejected as a duplicate"
    );
}
