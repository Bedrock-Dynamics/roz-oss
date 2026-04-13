//! Full-stack `DualPublishTransport` fanout test (ZEN-TEST-02 / gap #2).
//!
//! Composes NATS + Zenoh testcontainers, builds a `DualPublishTransport`
//! from real `NatsSessionTransport` + `ZenohSessionTransport` adapters, drives
//! `SessionTransport::publish_event_envelope`, and asserts fanout on BOTH
//! transports (raw NATS subject subscriber + raw Zenoh key-expr subscriber).
//!
//! Two `#[ignore]`-tagged tests run under nextest `ci-chaos` profile only
//! (`cargo nextest run --profile ci-chaos --run-ignored ignored-only`):
//!
//! 1. `dual_publish_fans_out_to_nats_and_zenoh` — happy path.
//! 2. `zenoh_degraded_leaves_nats_path_functional` — paused zenohd container
//!    must NOT break the NATS primary (D-19 non-fatal secondary semantics).
//!
//! Closes the "full-stack dual-publish" coverage gap flagged by
//! `.planning/phases/15-zenoh-edge-transport/15-VERIFICATION.md` (gap #2).
//! Extends the `session_relay_dual_publish_integration.rs` pattern (plan
//! 15-09) from in-process `CapturingTransport` to real dual-transport
//! composition against live containers.

#![allow(clippy::doc_markdown, clippy::items_after_statements, dead_code)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use ed25519_dalek::SigningKey;
use futures::StreamExt;
use rand::rngs::OsRng;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::transport::{DualPublishTransport, SessionTransport};
use roz_test::nats_container;
use roz_test::zenoh::{ZenohGuard, zenoh_router};
use roz_worker::transport_nats::NatsSessionTransport;
use roz_zenoh::session::ZenohSessionTransport;

/// Canonical shared fixture — byte-identical to plan 15-04 Task 3, plan 15-05
/// Task 1, and `session_relay_dual_publish_integration.rs` (D-18 wire-format
/// regression lock). The `correlation_id` also serves as the zenoh `session_id`
/// per `ZenohSessionTransport::envelope_routing` (team_id = "default").
fn fixture_envelope(event_id: &str, correlation_id: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId(event_id.into()),
        correlation_id: CorrelationId(correlation_id.into()),
        parent_event_id: None,
        // 2026-01-01T00:00:00Z
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

/// All the resources the two chaos tests share. Kept in one struct so the
/// test bodies read as assertion sequences.
struct Harness {
    _nats_guard: roz_test::NatsGuard,
    zenoh_guard: ZenohGuard,
    nats_client: async_nats::Client,
    dual: Arc<dyn SessionTransport>,
}

/// Boot both testcontainers, build `DualPublishTransport(NATS primary, Zenoh secondary)`.
/// Returns everything needed to subscribe on either side plus publish through the dual.
async fn setup_dual_transport() -> Harness {
    let nats_guard = nats_container().await;
    let zenoh_guard = zenoh_router().await;

    let signing_key = Arc::new(SigningKey::generate(&mut OsRng));

    let nats_client = async_nats::connect(nats_guard.url())
        .await
        .expect("nats connect");
    let nats_transport = NatsSessionTransport::new(nats_client.clone());

    let publisher_session = zenoh::open(zenoh_guard.peer_config())
        .await
        .expect("zenoh open pub session");
    let zenoh_transport =
        ZenohSessionTransport::open(publisher_session, signing_key, "worker-A".to_owned())
            .await
            .expect("ZenohSessionTransport::open");

    let dual: Arc<dyn SessionTransport> =
        Arc::new(DualPublishTransport::new(nats_transport, zenoh_transport));

    Harness {
        _nats_guard: nats_guard,
        zenoh_guard,
        nats_client,
        dual,
    }
}

/// Happy-path: with both NATS and zenohd reachable, a single
/// `publish_event_envelope` fans out to BOTH transports. Asserted by
/// subscribing on each side (pub/sub API per D-05, NOT log scraping).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (NATS + zenohd) — runs in ci-chaos nightly profile"]
async fn dual_publish_fans_out_to_nats_and_zenoh() {
    let h = setup_dual_transport().await;

    // Second zenoh peer for the raw subscriber side. Subscribe on the exact
    // key-expr literal `roz/sessions/{team_id}/{session_id}` (D-10) that
    // `ZenohSessionTransport::publish_event_envelope` writes to: team_id
    // defaults to "default" and session_id = correlation_id.
    let subscriber_session = zenoh::open(h.zenoh_guard.peer_config())
        .await
        .expect("zenoh open sub session");
    let zenoh_sub = subscriber_session
        .declare_subscriber("roz/sessions/default/corr-16-04-fixture")
        .await
        .expect("zenoh declare_subscriber");

    // NATS subscribe on the canonical event subject
    // (`roz.v1.session.<session_id>.events.<event_type>` per event_nats::event_subject).
    let env = fixture_envelope("evt-16-04-fixture", "corr-16-04-fixture");
    let nats_subject =
        roz_worker::event_nats::event_subject("roz.v1", &env.correlation_id.0, &env.event);
    let mut nats_sub = h
        .nats_client
        .subscribe(nats_subject.clone())
        .await
        .expect("nats sub");

    // Settle before publish (liveliness propagation — §8 pitfalls from 15-RESEARCH).
    tokio::time::sleep(Duration::from_millis(500)).await;

    h.dual
        .publish_event_envelope(&env)
        .await
        .expect("dual publish");

    // NATS side receives the envelope within 3s.
    let nats_msg = tokio::time::timeout(Duration::from_secs(3), nats_sub.next())
        .await
        .expect("nats recv timed out")
        .expect("nats channel closed");
    assert!(!nats_msg.payload.is_empty(), "nats payload empty");
    let got_via_nats: EventEnvelope =
        serde_json::from_slice(&nats_msg.payload).expect("decode nats payload");
    assert_eq!(got_via_nats.event_id.0, "evt-16-04-fixture");
    assert_eq!(got_via_nats.correlation_id.0, "corr-16-04-fixture");

    // Zenoh side receives the (signed) envelope within 3s.
    let zenoh_sample = tokio::time::timeout(Duration::from_secs(3), zenoh_sub.recv_async())
        .await
        .expect("zenoh recv timed out")
        .expect("zenoh channel closed");
    let zenoh_bytes = zenoh_sample.payload().to_bytes();
    assert!(!zenoh_bytes.is_empty(), "zenoh payload empty");
    // Zenoh payload is a `SignedSessionEnvelope` JSON wrapper around the
    // inner `EventEnvelope` (`ZenohSessionTransport::publish_event_envelope`
    // calls `sign_envelope` before `session.put`). We only need byte-level
    // reception here — signature verification is covered by
    // `crates/roz-zenoh/tests/signed_session_relay_integration.rs`.
    assert!(
        zenoh_bytes.len() > nats_msg.payload.len(),
        "zenoh payload ({} bytes) must be strictly larger than raw NATS envelope ({} bytes) \
         because it is a SignedSessionEnvelope wrapper",
        zenoh_bytes.len(),
        nats_msg.payload.len(),
    );
}

/// Degraded-path: pause the zenohd container mid-test. The NATS primary must
/// still succeed end-to-end because D-19 defines the Zenoh leg as a non-fatal
/// secondary — `DualPublishTransport::publish_event_envelope` logs the
/// secondary failure and swallows it rather than propagating.
///
/// Teardown: always `docker unpause` so subsequent test runs (and the
/// testcontainers drop) don't hit a frozen container.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (NATS + zenohd) — runs in ci-chaos nightly profile"]
async fn zenoh_degraded_leaves_nats_path_functional() {
    let h = setup_dual_transport().await;

    // Use a distinct correlation_id so the NATS subscription from this test
    // cannot collide with the happy-path test if both run in the same binary.
    let env = fixture_envelope("evt-16-04-degraded", "corr-16-04-degraded");
    let nats_subject =
        roz_worker::event_nats::event_subject("roz.v1", &env.correlation_id.0, &env.event);
    let mut nats_sub = h
        .nats_client
        .subscribe(nats_subject.clone())
        .await
        .expect("nats sub");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pause the zenohd container to simulate a hard partition. testcontainers
    // 0.27 doesn't expose a first-class `pause()` method, so shell out via
    // `docker pause <id>`. Container id is plumbed through
    // `ZenohGuard::container_id()` (added in Phase 16 plan 16-04).
    let zenoh_container_id = h
        .zenoh_guard
        .container_id()
        .expect("zenoh_router() must start a real container (no ZENOH_ROUTER_ENDPOINT bypass)");
    let pause_status = tokio::process::Command::new("docker")
        .args(["pause", &zenoh_container_id])
        .status()
        .await
        .expect("docker pause spawn");
    assert!(
        pause_status.success(),
        "docker pause exited non-zero: {pause_status}"
    );

    // With zenohd frozen, the secondary transport's `put` will block or fail.
    // Per D-19 non-fatal secondary semantics, the dual-publish call must
    // still return Ok — primary (NATS) succeeds and the secondary failure is
    // logged-and-swallowed.
    let publish_result =
        tokio::time::timeout(Duration::from_secs(10), h.dual.publish_event_envelope(&env)).await;

    // Cleanup FIRST so a surprising publish failure still unfreezes the
    // container for the testcontainers Drop impl. The unpause is best-effort
    // (we ignore its exit code — if docker itself has died there's nothing
    // to clean up).
    let _unpause = tokio::process::Command::new("docker")
        .args(["unpause", &zenoh_container_id])
        .status()
        .await;

    // Now assert the publish outcome.
    publish_result
        .expect("dual publish timed out — D-19 regression: secondary failure is blocking primary")
        .expect("dual publish returned Err — D-19 regression: zenoh secondary failure propagated");

    // NATS primary must have delivered the envelope even with zenohd paused.
    let nats_msg = tokio::time::timeout(Duration::from_secs(5), nats_sub.next())
        .await
        .expect("nats recv timed out despite being primary")
        .expect("nats channel closed");
    assert!(!nats_msg.payload.is_empty(), "nats payload empty");
    let got: EventEnvelope =
        serde_json::from_slice(&nats_msg.payload).expect("decode nats payload");
    assert_eq!(got.event_id.0, "evt-16-04-degraded");
    assert_eq!(got.correlation_id.0, "corr-16-04-degraded");
}
