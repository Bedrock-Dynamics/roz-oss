//! Full-stack `DualPublishTransport` fanout test (ZEN-TEST-02 / gap #2).
//!
//! Composes NATS + Zenoh testcontainers, builds a `DualPublishTransport`
//! from real `NatsSessionTransport` + `ZenohSessionTransport` adapters, drives
//! `SessionTransport::publish_event_envelope`, and asserts fanout on BOTH
//! transports (raw NATS subject subscriber + raw Zenoh key-expr subscriber).
//!
//! `#[ignore]`-tagged — runs under nextest `ci-chaos` profile only
//! (`cargo nextest run --profile ci-chaos --run-ignored ignored-only`).
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
use rand::rngs::OsRng;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::transport::{DualPublishTransport, SessionTransport};
use roz_test::{nats_container, zenoh::zenoh_router};
use roz_worker::transport_nats::NatsSessionTransport;
use roz_zenoh::session::ZenohSessionTransport;

/// Canonical shared fixture — byte-identical to plan 15-04 Task 3, plan 15-05
/// Task 1, and `session_relay_dual_publish_integration.rs` (D-18 wire-format
/// regression lock). The `correlation_id` also serves as the zenoh `session_id`
/// per `ZenohSessionTransport::envelope_routing` (team_id = "default").
fn fixture_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-16-04-fixture".into()),
        correlation_id: CorrelationId("corr-16-04-fixture".into()),
        parent_event_id: None,
        // 2026-01-01T00:00:00Z
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

/// Happy-path: with both NATS and zenohd reachable, a single
/// `publish_event_envelope` fans out to BOTH transports. Asserted by
/// subscribing on each side (pub/sub API per D-05, NOT log scraping).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (NATS + zenohd) — runs in ci-chaos nightly profile"]
async fn dual_publish_fans_out_to_nats_and_zenoh() {
    // 1. Boot NATS + zenohd testcontainers.
    let nats = nats_container().await;
    let zenoh = zenoh_router().await;

    // 2. Ephemeral signing key for the ZenohSessionTransport (D-22 signed envelopes).
    let signing_key = Arc::new(SigningKey::generate(&mut OsRng));

    // 3. Connect NATS + build NatsSessionTransport primary.
    let nats_client = async_nats::connect(nats.url()).await.expect("nats connect");
    let nats_transport = NatsSessionTransport::new(nats_client.clone());

    // 4. Open a zenoh peer session for the publisher transport + build ZenohSessionTransport secondary.
    let publisher_session = zenoh::open(zenoh.peer_config())
        .await
        .expect("zenoh open pub session");
    let zenoh_transport = ZenohSessionTransport::open(
        publisher_session,
        signing_key.clone(),
        "worker-A".to_owned(),
    )
    .await
    .expect("ZenohSessionTransport::open");

    // 5. Second zenoh peer for the raw subscriber side. We subscribe on the
    //    exact key-expr literal `roz/sessions/{team_id}/{session_id}` (D-10)
    //    that `ZenohSessionTransport::publish_event_envelope` writes to:
    //    team_id defaults to "default" and session_id = correlation_id.
    let subscriber_session = zenoh::open(zenoh.peer_config())
        .await
        .expect("zenoh open sub session");
    let zenoh_sub = subscriber_session
        .declare_subscriber("roz/sessions/default/corr-16-04-fixture")
        .await
        .expect("zenoh declare_subscriber");

    // 6. NATS subscribe on the canonical event subject
    //    (`roz.v1.session.<session_id>.events.<event_type>` per event_nats::event_subject).
    let env = fixture_envelope();
    let nats_subject =
        roz_worker::event_nats::event_subject("roz.v1", &env.correlation_id.0, &env.event);
    let mut nats_sub = nats_client
        .subscribe(nats_subject.clone())
        .await
        .expect("nats sub");

    // 7. Let zenoh peer sessions + NATS subscribe settle before publish
    //    (liveliness propagation — §8 pitfalls from 15-RESEARCH).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 8. Compose DualPublishTransport(NATS primary, Zenoh secondary).
    let dual: Arc<dyn SessionTransport> =
        Arc::new(DualPublishTransport::new(nats_transport, zenoh_transport));

    // 9. Publish through the composed transport.
    dual.publish_event_envelope(&env)
        .await
        .expect("dual publish");

    // 10. Assert NATS side receives the envelope within 3s.
    use futures::StreamExt;
    let nats_msg = tokio::time::timeout(Duration::from_secs(3), nats_sub.next())
        .await
        .expect("nats recv timed out")
        .expect("nats channel closed");
    assert!(!nats_msg.payload.is_empty(), "nats payload empty");
    let got_via_nats: EventEnvelope =
        serde_json::from_slice(&nats_msg.payload).expect("decode nats payload");
    assert_eq!(got_via_nats.event_id.0, "evt-16-04-fixture");
    assert_eq!(got_via_nats.correlation_id.0, "corr-16-04-fixture");

    // 11. Assert Zenoh side receives the (signed) envelope within 3s.
    let zenoh_sample = tokio::time::timeout(Duration::from_secs(3), zenoh_sub.recv_async())
        .await
        .expect("zenoh recv timed out")
        .expect("zenoh channel closed");
    let zenoh_bytes = zenoh_sample.payload().to_bytes();
    assert!(!zenoh_bytes.is_empty(), "zenoh payload empty");
    // The Zenoh payload is a `SignedSessionEnvelope` JSON wrapper around the
    // inner `EventEnvelope` (ZenohSessionTransport::publish_event_envelope
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
