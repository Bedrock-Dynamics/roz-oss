//! Worker startup sanity (ZEN-TEST-01 / gap #1).
//!
//! Spawns ONE real `roz-worker` binary (via fleet helper), subscribes from a
//! test peer to `roz/<worker_id>/transport/health` and
//! `roz/coordination/pose/<worker_id>`, asserts the startup samples arrive
//! with the expected payload shape.
//!
//! Closes Phase 15-VERIFICATION human_verification item #4 (runtime-observable
//! startup samples). Uses pub/sub assertions per D-05 — no log scraping.
//!
//! `#[ignore]`-tagged — runs in nextest ci-chaos profile only.

#![cfg(feature = "zenoh")]
#![allow(clippy::doc_markdown, clippy::items_after_statements)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_test::{nats_container, zenoh_router};

use crate::common::fleet::{shutdown_worker, spawn_worker};

/// Persist a fresh Ed25519 seed to a tempfile whose directory outlives the
/// test (`roz_zenoh::envelope::load_signing_key` accepts a raw-32-byte file).
fn write_signing_key_to_tempfile(key: &SigningKey) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("signing.key");
    let seed = key.to_bytes();
    std::fs::write(&path, seed).expect("write key");
    (dir, path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns roz-worker binary + requires Docker — ci-chaos nightly only"]
async fn worker_startup_publishes_transport_health_ready_and_zero_pose() {
    let nats = nats_container().await;
    let zenoh = zenoh_router().await;

    // 1. Generate ephemeral signing key, write to tempfile. `_keydir` retains
    //    the TempDir so the file is not removed while the worker reads it.
    let key = SigningKey::generate(&mut OsRng);
    let (_keydir, key_path) = write_signing_key_to_tempfile(&key);

    let worker_id = format!("test-worker-{}", uuid::Uuid::new_v4());

    // 2. Subscribe from a test peer BEFORE spawning the worker so the startup
    //    sample is not missed due to subscribe-after-publish races (pitfall
    //    §8: liveliness propagation race).
    let sub_session = zenoh::open(zenoh.peer_config()).await.expect("open sub session");

    let health_key = format!("roz/{worker_id}/transport/health");
    let pose_key = format!("roz/coordination/pose/{worker_id}");

    let health_sub = sub_session
        .declare_subscriber(&health_key)
        .await
        .expect("declare health sub");
    let pose_sub = sub_session
        .declare_subscriber(&pose_key)
        .await
        .expect("declare pose sub");

    // 3. Settle — 500ms for subscriber declarations to propagate through the
    //    router before the worker starts publishing.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. Spawn the worker against the shared router + NATS container.
    let worker = spawn_worker(
        &worker_id,
        zenoh.tcp_endpoint(),
        nats.url(),
        key_path.to_str().expect("key path utf-8"),
    )
    .await
    .expect("spawn worker");

    // 5. Assert TRANSPORT_HEALTH startup sample within 30s. The worker
    //    publishes BOTH the 15-10 startup rollup (`{status:"ready", source:
    //    "edge_state_bus_runner::startup", worker_id}`) and the 15-06
    //    continuous heartbeat (`EdgeTransportHealth::Healthy` serialized as
    //    `{status:"healthy"}`). Order is not guaranteed on a fresh zenoh
    //    session — consume until we see the startup rollup identified by its
    //    `source` field, then assert its shape.
    let health_json = {
        let mut found: Option<serde_json::Value> = None;
        let loop_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < loop_deadline {
            let remaining = loop_deadline - tokio::time::Instant::now();
            let sample = tokio::time::timeout(remaining, health_sub.recv_async())
                .await
                .expect("TRANSPORT_HEALTH startup sample timed out — worker did not publish within 30s")
                .expect("health subscriber channel closed");
            let bytes = sample.payload().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).expect("transport/health payload is not JSON");
            if json["source"] == "edge_state_bus_runner::startup" {
                found = Some(json);
                break;
            }
            // Otherwise it's the 15-06 continuous heartbeat — keep waiting
            // for the 15-10 startup rollup.
        }
        found.expect("never observed 15-10 startup rollup (source=edge_state_bus_runner::startup) within 30s")
    };
    assert_eq!(
        health_json["status"], "ready",
        "expected status=ready in startup rollup, got {health_json}"
    );
    assert_eq!(
        health_json["worker_id"], worker_id,
        "worker_id field must echo the spawned ROZ_WORKER_ID, got {health_json}",
    );

    // 6. Assert zero RobotPose startup sample within 30s. RobotPose serializes
    //    via serde_json (coordination.rs:80: `serde_json::to_vec(pose)`), so
    //    the payload IS valid JSON with {robot_id, position, orientation, timestamp_ns}.
    let pose_sample = tokio::time::timeout(Duration::from_secs(30), pose_sub.recv_async())
        .await
        .expect("pose startup sample timed out")
        .expect("pose subscriber channel closed");
    let pose_bytes = pose_sample.payload().to_bytes();
    assert!(!pose_bytes.is_empty(), "pose payload empty");
    let pose_json: serde_json::Value = serde_json::from_slice(&pose_bytes).expect("pose payload is not JSON");
    assert_eq!(pose_json["robot_id"], worker_id, "pose robot_id echoes worker_id");
    // Zero-pose is defined by 15-10: position=[0,0,0], orientation=[1,0,0,0].
    let pos = pose_json["position"].as_array().expect("position is array");
    assert_eq!(pos.len(), 3, "position has 3 components");
    for (i, v) in pos.iter().enumerate() {
        assert_eq!(v.as_f64(), Some(0.0), "startup pose position[{i}] must be 0.0, got {v}");
    }
    let ori = pose_json["orientation"].as_array().expect("orientation is array");
    assert_eq!(ori.len(), 4, "orientation has 4 components");
    assert_eq!(
        ori[0].as_f64(),
        Some(1.0),
        "startup pose orientation w (quat real part) must be 1.0",
    );

    shutdown_worker(worker).await;
}
