//! ZEN-02 + T-01 mitigation: signed `SessionEvent` relay + forged-envelope
//! rejection against a real zenohd testcontainer.
//!
//! Covers:
//! - Two-peer signed publish/subscribe via `ZenohSessionTransport`.
//! - T-01 mitigation: an envelope signed by a key NOT advertised via
//!   liveliness never reaches the subscriber.
//!
//! Per D-29 no `#[ignore]` — tests run in the default `cargo test` matrix
//! gated only by Docker availability.

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::transport::SessionTransport;
use roz_test::zenoh::zenoh_router;
use roz_zenoh::envelope::{SignedSessionEnvelope, sign_envelope};
use roz_zenoh::session::ZenohSessionTransport;

/// Canonical shared fixture — byte-identical to plan 15-04 Task 1/3 and plan
/// 15-05 Task 1. Do NOT edit the field values; drift breaks the D-18
/// wire-format regression lock.
fn fixture_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-15-fixture".into()),
        correlation_id: CorrelationId("corr-15-fixture".into()),
        parent_event_id: None,
        // 2026-01-01T00:00:00Z
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_envelope_roundtrip() {
    let router = zenoh_router().await;

    let key_a = Arc::new(SigningKey::generate(&mut OsRng));
    let key_b = Arc::new(SigningKey::generate(&mut OsRng));

    let sess_a = zenoh::open(router.peer_config()).await.unwrap();
    let sess_b = zenoh::open(router.peer_config()).await.unwrap();

    let transport_a = ZenohSessionTransport::open(sess_a, key_a, "robot-a".into())
        .await
        .unwrap();
    let transport_b = ZenohSessionTransport::open(sess_b, key_b, "robot-b".into())
        .await
        .unwrap();

    // Wait for liveliness propagation + identity bootstrap so peers have each
    // other's pubkey in cache before any signed publish.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // C-11 (15-05): `subscribe_session_raw` is a concrete method on
    // `ZenohSessionTransport` with `pub rx: Receiver<EventEnvelope>`.
    //
    // Routing: `envelope_routing` uses `team_id = "default"` and sets
    // `session_id = envelope.correlation_id.0`. Align the subscribe key with
    // the canonical fixture's `correlation_id`.
    let mut raw_sub = transport_b
        .subscribe_session_raw("tenant", "default", "corr-15-fixture")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let envelope = fixture_envelope();
    transport_a.publish_event_envelope(&envelope).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(3), raw_sub.rx.recv())
        .await
        .expect("recv timed out")
        .expect("channel closed");
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        serde_json::to_value(&envelope).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_envelope_is_dropped() {
    let router = zenoh_router().await;

    let key_a = Arc::new(SigningKey::generate(&mut OsRng));
    let key_b = Arc::new(SigningKey::generate(&mut OsRng));
    // Forger key — never advertised via liveliness, so peer_keys cache on
    // subscriber never contains it. `verify_envelope` must reject with
    // "unknown peer pubkey".
    let forged_key = SigningKey::generate(&mut OsRng);

    let sess_a = zenoh::open(router.peer_config()).await.unwrap();
    let sess_b = zenoh::open(router.peer_config()).await.unwrap();
    let sess_forger = zenoh::open(router.peer_config()).await.unwrap();

    let transport_a = ZenohSessionTransport::open(sess_a, key_a, "robot-a".into())
        .await
        .unwrap();
    let transport_b = ZenohSessionTransport::open(sess_b, key_b, "robot-b".into())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    // `envelope_routing`: team_id = "default", session_id = correlation_id.
    // Align the subscribe key with the correlation_id used below.
    let session_id = "session-2";
    let mut raw_sub = transport_b
        .subscribe_session_raw("tenant", "default", session_id)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Publish a forged envelope directly via a raw zenoh::Session (the forger
    // has no `ZenohSessionTransport` and never advertises its key via
    // liveliness, so the subscriber's PeerKeyCache never learns it).
    let mut forged_envelope = fixture_envelope();
    forged_envelope.correlation_id = CorrelationId(session_id.into());
    let forged: SignedSessionEnvelope = sign_envelope(&forged_key, &forged_envelope).unwrap();
    let forged_bytes = serde_json::to_vec(&forged).unwrap();
    sess_forger
        .put(format!("roz/sessions/default/{session_id}"), forged_bytes)
        .await
        .unwrap();

    // Publish a legitimate envelope from transport_a — also routed to
    // session-2 via correlation_id.
    let mut legit_envelope = fixture_envelope();
    legit_envelope.correlation_id = CorrelationId(session_id.into());
    transport_a.publish_event_envelope(&legit_envelope).await.unwrap();

    // Drain any samples within a bounded window. Only legitimately-signed
    // envelopes (key_a, advertised via liveliness) must reach the receiver.
    let mut seen = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), raw_sub.rx.recv()).await {
            Ok(Some(env)) => {
                assert_eq!(
                    serde_json::to_value(&env).unwrap(),
                    serde_json::to_value(&legit_envelope).unwrap(),
                    "received envelope must decode to the legitimate one",
                );
                seen += 1;
            }
            _ => break,
        }
    }
    assert_eq!(
        seen, 1,
        "expected exactly 1 legitimate envelope, got {seen} — forged one leaked through",
    );
}
