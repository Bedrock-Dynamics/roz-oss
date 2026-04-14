//! Multi-worker cross-process test suite (ZEN-TEST-03 / gap #3).
//!
//! Spawns N=3 real `roz-worker` binaries (D-04 raft-quorum convention) against
//! a single shared zenohd testcontainer. Verifies:
//!   - Liveliness-based peer discovery within the worker-startup budget
//!   - Barrier late-joiner semantics across three distinct `zenoh::Session`
//!     instances (proxy for cross-process; in-process coverage lives in
//!     `crates/roz-zenoh/tests/coordination_integration.rs`).
//!
//! Cross-process Ed25519 pubkey bootstrap is covered indirectly: the liveliness
//! mechanism itself IS the pubkey-bootstrap carrier per 15-05 (liveliness token
//! on `roz/peers/<id>` triggers the identity queryable on
//! `roz/peers/<id>/identity`). Observing the liveliness tokens from 3 distinct
//! processes therefore validates that `ZenohSessionTransport::open` ran to
//! completion — which requires successfully loading the per-worker signing key
//! and publishing the associated `PeerAnnouncement` / identity queryable.
//!
//! `#[ignore]`-tagged — runs in ci-chaos nightly profile only.

#![cfg(feature = "zenoh")]
#![allow(clippy::doc_markdown, clippy::items_after_statements)]

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_test::{nats_container, zenoh_router};
use roz_zenoh::coordination::ZenohCoordinator;

use crate::common::fleet::{shutdown_worker, spawn_worker};

/// Persist a fresh Ed25519 seed to a tempfile whose directory outlives the
/// test. `roz_zenoh::envelope::load_signing_key` accepts the raw 32-byte file.
fn write_signing_key_to_tempfile(key: &SigningKey) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("signing.key");
    std::fs::write(&path, key.to_bytes()).expect("write key");
    (dir, path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns 3 roz-worker binaries + Docker — ci-chaos nightly only"]
async fn three_workers_discover_each_other_via_liveliness() {
    let nats = nats_container().await;
    let zenoh = zenoh_router().await;

    // 1. Subscribe to the liveliness space BEFORE spawning any worker so no
    //    token announcement is missed. Zenoh liveliness also replays currently-
    //    alive tokens to new subscribers, so the order is strictly defensive.
    let obs_session = zenoh::open(zenoh.peer_config()).await.expect("open obs session");
    let live_sub = obs_session
        .liveliness()
        .declare_subscriber("roz/peers/*")
        .await
        .expect("declare liveliness subscriber");

    // Settle — liveliness subscriber declare must propagate through the router.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. Generate N=3 distinct worker IDs + signing keys; spawn all three.
    let ids: Vec<String> = (0..3).map(|i| format!("fleet-{i}-{}", uuid::Uuid::new_v4())).collect();
    let mut workers = Vec::new();
    // Retain the TempDirs for the full test lifetime so the per-worker
    // signing-key files survive until after the workers have read them
    // (Drop on TempDir removes the directory).
    let mut key_dirs: Vec<tempfile::TempDir> = Vec::new();
    for id in &ids {
        let key = SigningKey::generate(&mut OsRng);
        let (dir, path) = write_signing_key_to_tempfile(&key);
        let w = spawn_worker(
            id,
            zenoh.tcp_endpoint(),
            nats.url(),
            path.to_str().expect("key path utf-8"),
        )
        .await
        .expect("spawn worker");
        workers.push(w);
        key_dirs.push(dir);
    }

    // 3. Observe liveliness tokens for all 3 ids within 30s. Each worker's
    //    `ZenohSessionTransport::open` declares a token on `roz/peers/<id>`
    //    (see roz-zenoh/src/session.rs:279-284); 30s budget is ≥10× the
    //    observed worker startup (NATS connect + host register + zenoh open).
    let mut seen: HashSet<String> = HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while seen.len() < 3 && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, live_sub.recv_async()).await {
            Ok(Ok(sample)) => {
                let key = sample.key_expr().as_str().to_owned();
                // Extract id: "roz/peers/<id>" — reject deeper segments.
                if let Some(id_rest) = key.strip_prefix("roz/peers/")
                    && !id_rest.is_empty()
                    && !id_rest.contains('/')
                {
                    seen.insert(id_rest.to_owned());
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert_eq!(seen.len(), 3, "expected 3 liveliness tokens within 30s, saw {seen:?}");
    for id in &ids {
        assert!(
            seen.contains(id),
            "missing liveliness token for worker id {id}, saw {seen:?}",
        );
    }

    // 4. Teardown — SIGTERM all three in parallel.
    let teardown = workers.into_iter().map(|w| tokio::spawn(shutdown_worker(w)));
    futures::future::join_all(teardown).await;
    // Explicit drop: release the signing-key TempDirs only after teardown.
    drop(key_dirs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (zenohd) — ci-chaos nightly only"]
async fn barrier_late_joiner_synchronizes_with_existing_participants() {
    let zenoh = zenoh_router().await;

    let barrier_name = format!("phase16-{}", uuid::Uuid::new_v4());

    // Open three distinct zenoh::Session instances — proxy for three separate
    // processes (the same-address-space constraint is what differs from a
    // truly cross-process test, but the barrier mechanism queries + liveliness
    // both route through the router, so the observable behaviour is the same).
    let sess_a = zenoh::open(zenoh.peer_config()).await.expect("open A");
    let sess_b = zenoh::open(zenoh.peer_config()).await.expect("open B");
    let sess_c = zenoh::open(zenoh.peer_config()).await.expect("open C");

    // A + B join first.
    let _guard_a = ZenohCoordinator::join_barrier(&sess_a, &barrier_name, "peer-a")
        .await
        .expect("A join");
    let _guard_b = ZenohCoordinator::join_barrier(&sess_b, &barrier_name, "peer-b")
        .await
        .expect("B join");

    // A also acts as the queryable-server for the barrier (late joiners hit
    // `roz/coordination/barrier/<name>` via session.get and A replies with
    // the current participant set).
    let (participants_a, _obs_task) = ZenohCoordinator::observe_barrier(sess_a.clone(), barrier_name.clone())
        .await
        .expect("observe A");
    let _q_task = ZenohCoordinator::declare_barrier_queryable(sess_a, barrier_name.clone(), participants_a.clone())
        .await
        .expect("declare queryable");

    // Settle — liveliness propagation + queryable declare.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Late joiner C queries existing participants BEFORE joining (this is the
    // late-joiner semantic: a fresh peer must be able to learn the current
    // barrier membership without itself having joined).
    let existing = tokio::time::timeout(
        Duration::from_secs(5),
        ZenohCoordinator::query_barrier_participants(&sess_c, &barrier_name),
    )
    .await
    .expect("query participants timeout")
    .expect("query error");
    let existing_set: HashSet<String> = existing.into_iter().collect();
    assert!(
        existing_set.contains("peer-a"),
        "late-joiner C did not see peer-a via queryable, got {existing_set:?}",
    );
    assert!(
        existing_set.contains("peer-b"),
        "late-joiner C did not see peer-b via queryable, got {existing_set:?}",
    );

    // C joins.
    let _guard_c = ZenohCoordinator::join_barrier(&sess_c, &barrier_name, "peer-c")
        .await
        .expect("C join");

    // All three should converge to a size-3 participant set within 10s via
    // the liveliness subscriber that `observe_barrier` spawned on A.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            let snap: Vec<String> = participants_a.read().iter().cloned().collect();
            panic!("observer did not reach 3 participants within 10s, final snapshot {snap:?}");
        }
        let len = participants_a.read().len();
        if len == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let final_set: HashSet<String> = participants_a.read().iter().cloned().collect();
    assert!(final_set.contains("peer-a"));
    assert!(final_set.contains("peer-b"));
    assert!(final_set.contains("peer-c"));
}
