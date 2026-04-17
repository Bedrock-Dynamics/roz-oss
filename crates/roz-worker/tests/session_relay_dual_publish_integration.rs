//! ZEN-02 gap closure: proves `event_transport` is actually invoked from the
//! session_relay publish pathway (not just from `ZenohSessionTransport` in
//! isolation). Verifies the `DualPublishTransport` composition landing in
//! main.rs propagates all the way through `spawn_session_relay`'s internal
//! helpers.
//!
//! This is the gap `15-VERIFICATION.md` flagged: the existing
//! `signed_session_relay_integration.rs` in roz-zenoh exercises
//! `ZenohSessionTransport` directly and bypasses the worker session relay.
//! These tests close that blind spot by driving `publish_event_envelope_for_test`
//! (which mirrors the production helper) with a `DualPublishTransport`
//! (NATS primary, in-process `CapturingTransport` secondary) and asserting that:
//!   1. the capturing secondary receives the exact envelope, AND
//!   2. the NATS `event_subject` leg still fires (D-18 byte-stable).

#![allow(clippy::doc_markdown, clippy::items_after_statements)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::DateTime;
use futures::StreamExt;
use roz_core::session::event::{
    CanonicalSessionEventEnvelope, CorrelationId, EventEnvelope, EventId, SessionEvent, canonical_event_type_name,
};
use roz_core::transport::{DualPublishTransport, SessionTransport};
use roz_worker::session_relay::publish_event_envelope_for_test;
use roz_worker::transport_nats::NatsSessionTransport;

struct CapturingTransport {
    count: Arc<AtomicUsize>,
    captured: Arc<parking_lot::Mutex<Vec<EventEnvelope>>>,
}

#[async_trait]
impl SessionTransport for CapturingTransport {
    async fn publish_event_envelope(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.captured.lock().push(envelope.clone());
        Ok(())
    }
}

// Canonical shared fixture (byte-identical to session_transport_regression;
// D-18 wire-format lock).
fn fixture_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-15-fixture".into()),
        correlation_id: CorrelationId("corr-15-fixture".into()),
        parent_event_id: None,
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(), // 2026-01-01T00:00:00Z
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

fn skill_loaded_fixture() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-21-1-skill-loaded".into()),
        correlation_id: CorrelationId("corr-21-1-skill-loaded".into()),
        parent_event_id: None,
        timestamp: DateTime::from_timestamp(1_776_297_600, 0).unwrap(), // 2026-04-16T00:00:00Z
        event: SessionEvent::SkillLoaded {
            name: "warehouse-skill".into(),
            version: "0.1.0".into(),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_publish_flows_through_session_relay() {
    // Arrange: NATS container + DualPublishTransport(NATS primary, capturing secondary).
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.expect("nats connect");

    let count = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let capturing = CapturingTransport {
        count: count.clone(),
        captured: captured.clone(),
    };

    let nats_transport = NatsSessionTransport::new(nats.clone());
    let dual: Arc<dyn SessionTransport> = Arc::new(DualPublishTransport::new(nats_transport, capturing));

    // Subscribe on NATS side to confirm the inline event_subject publish still fires.
    let env = fixture_envelope();
    let expected_subject = roz_worker::event_nats::event_subject("roz.v1", "corr-15-fixture", &env.event);
    let mut sub = nats.subscribe(expected_subject.clone()).await.expect("sub");

    // Act: call publish_event_envelope_for_test with dual transport injected.
    publish_event_envelope_for_test(
        &nats,
        "corr-15-fixture",
        "roz.v1.session.test.response",
        &env,
        Some(&dual),
    )
    .await
    .expect("publish");

    // Assert A: secondary (capturing) transport received the envelope exactly once.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "capturing transport must be invoked exactly once via session_relay pathway"
    );
    // Clone captured envelopes out of the mutex so the guard drops immediately
    // (avoids clippy::await_holding_lock + clippy::significant_drop_tightening
    // across the NATS recv await below).
    let captured_snapshot: Vec<EventEnvelope> = captured.lock().clone();
    assert_eq!(captured_snapshot.len(), 1);
    assert_eq!(captured_snapshot[0].event_id.0, "evt-15-fixture");
    assert_eq!(captured_snapshot[0].correlation_id.0, "corr-15-fixture");

    // Assert B: NATS path still fires (D-18 regression lock).
    let msg = tokio::time::timeout(Duration::from_secs(3), sub.next())
        .await
        .expect("nats recv timeout")
        .expect("nats msg");
    let got_env: EventEnvelope = serde_json::from_slice(&msg.payload).expect("decode");
    assert_eq!(got_env.event_id.0, "evt-15-fixture");
    assert_eq!(got_env.correlation_id.0, "corr-15-fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn none_event_transport_preserves_nats_only_path() {
    // D-18 regression: when event_transport is None, the inline NATS publish
    // behaves exactly as pre-15-04 (no transport invocation, NATS subject unchanged).
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.expect("nats connect");

    let env = fixture_envelope();
    let expected_subject = roz_worker::event_nats::event_subject("roz.v1", "corr-15-fixture", &env.event);
    let mut sub = nats.subscribe(expected_subject).await.expect("sub");

    publish_event_envelope_for_test(&nats, "corr-15-fixture", "roz.v1.session.test.response", &env, None)
        .await
        .expect("publish");

    let msg = tokio::time::timeout(Duration::from_secs(3), sub.next())
        .await
        .expect("nats recv timeout")
        .expect("nats msg");
    let got_env: EventEnvelope = serde_json::from_slice(&msg.payload).expect("decode");
    assert_eq!(got_env.event_id.0, "evt-15-fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_event_publish_preserves_correlation_across_worker_relay_legs() {
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.expect("nats connect");

    let count = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let capturing = CapturingTransport {
        count: count.clone(),
        captured: captured.clone(),
    };
    let nats_transport = NatsSessionTransport::new(nats.clone());
    let dual: Arc<dyn SessionTransport> = Arc::new(DualPublishTransport::new(nats_transport, capturing));

    let env = skill_loaded_fixture();
    let response_subject = "roz.v1.session.test.skill.response";
    let event_subject = roz_worker::event_nats::event_subject("roz.v1", "corr-21-1-skill-loaded", &env.event);
    let mut response_sub = nats.subscribe(response_subject).await.expect("response sub");
    let mut event_sub = nats.subscribe(event_subject).await.expect("event sub");

    publish_event_envelope_for_test(&nats, "corr-21-1-skill-loaded", response_subject, &env, Some(&dual))
        .await
        .expect("publish");

    let canonical_msg = tokio::time::timeout(Duration::from_secs(3), response_sub.next())
        .await
        .expect("canonical recv timeout")
        .expect("canonical msg");
    let canonical: CanonicalSessionEventEnvelope =
        serde_json::from_slice(&canonical_msg.payload).expect("decode canonical");
    assert_eq!(canonical.correlation_id, "corr-21-1-skill-loaded");
    assert_eq!(canonical.event_type, "skill_loaded");

    let event_msg = tokio::time::timeout(Duration::from_secs(3), event_sub.next())
        .await
        .expect("event recv timeout")
        .expect("event msg");
    let relayed: EventEnvelope = serde_json::from_slice(&event_msg.payload).expect("decode event envelope");
    assert_eq!(relayed.correlation_id.0, "corr-21-1-skill-loaded");
    assert_eq!(canonical_event_type_name(&relayed.event), "skill_loaded");

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "capturing transport must still be invoked for the skill event"
    );
    let captured_snapshot: Vec<EventEnvelope> = captured.lock().clone();
    assert_eq!(captured_snapshot.len(), 1);
    assert_eq!(captured_snapshot[0].correlation_id.0, "corr-21-1-skill-loaded");
    assert_eq!(canonical_event_type_name(&captured_snapshot[0].event), "skill_loaded");
}
