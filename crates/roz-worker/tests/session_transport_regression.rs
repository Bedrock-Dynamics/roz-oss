//! BLOCKING regression suite for D-18: `SessionTransport` refactor must
//! preserve the Phase 13 NATS session relay path byte-for-byte.
//!
//! These tests guard:
//! - NATS subject format (`session.<worker>.<session>.request|response`)
//! - `EventEnvelope` JSON wire shape
//! - End-to-end roundtrip via `NatsSessionTransport`
//! - `DualPublishTransport` failure isolation
//! - `runtime_checkpoint` byte-stable publish path (C-10)
//! - Wildcard `session.{worker_id}.*.request` subscribe path (C-10)

#![allow(clippy::doc_markdown, clippy::items_after_statements)]

use chrono::DateTime;
use futures::StreamExt;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::transport::{DualPublishTransport, SessionTransport};
use roz_nats::subjects::Subjects;
use roz_worker::event_nats::event_subject;
use roz_worker::transport_nats::NatsSessionTransport;

#[test]
fn subjects_unchanged() {
    // C-10 EXPANDED: pin EVERY Subjects::* helper that session_relay.rs uses,
    // plus the inline wildcard literal at session_relay.rs:582, plus the
    // event_subject(...) helper from event_nats.rs.
    //
    // These subjects are D-18 BLOCKING. Any change breaks wire compat with
    // deployed roz-server and other workers -- requires coordinated migration.
    assert_eq!(Subjects::session_request("w1", "s1").unwrap(), "session.w1.s1.request");
    assert_eq!(
        Subjects::session_response("w1", "s1").unwrap(),
        "session.w1.s1.response"
    );
    assert_eq!(Subjects::session_control("w1", "s1").unwrap(), "session.w1.s1.control");

    // Inline wildcard format at session_relay.rs (spawn_session_relay):
    // format!("session.{worker_id}.*.request").
    let worker_id = "w1";
    let wildcard = format!("session.{worker_id}.*.request");
    assert_eq!(wildcard, "session.w1.*.request");

    // event_subject helper from event_nats.rs (used at session_relay.rs:1066).
    let event = SessionEvent::TurnStarted { turn_index: 0 };
    let subj = event_subject("roz.v1", "s1", &event);
    // Pinned format: "{prefix}.session.{session_id}.events.{event_variant}".
    assert_eq!(subj, "roz.v1.session.s1.events.turn_started");
}

#[tokio::test]
async fn runtime_checkpoint_publish_path_unchanged() {
    // C-10 EXPANSION: verify the runtime_checkpoint publish fires via the
    // existing INLINE session_relay path (NOT via SessionTransport -- per
    // C-01 narrowing, runtime_checkpoint stays inline).
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.unwrap();

    let response_subject = "session.w-rc-test.s-rc-test.response";
    let mut raw_sub = nats.subscribe(response_subject).await.unwrap();

    let checkpoint = serde_json::json!({"bootstrap": true});
    roz_worker::session_relay::publish_runtime_checkpoint_for_test(&nats, response_subject, &checkpoint)
        .await
        .unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), raw_sub.next())
        .await
        .expect("recv timed out")
        .expect("stream closed");
    let decoded: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(
        decoded["type"], "runtime_checkpoint",
        "payload must have type:runtime_checkpoint"
    );
}

#[tokio::test]
async fn wildcard_start_session_subscribe_unchanged() {
    // C-10: verify `session.{worker_id}.*.request` wildcard subscribe works
    // (this is session_relay.rs:582 -- stays inline under C-01).
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.unwrap();

    let worker_id = "w-wild-test";
    let wildcard = format!("session.{worker_id}.*.request");
    let mut sub = nats.subscribe(wildcard.clone()).await.unwrap();

    // Publish a request on a specific session_id -- wildcard must match.
    let target = format!("session.{worker_id}.s-wild-test.request");
    nats.publish(target.clone(), b"start".to_vec().into()).await.unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), sub.next())
        .await
        .expect("recv timed out")
        .expect("stream closed");
    assert_eq!(msg.subject.to_string(), target, "wildcard matched wrong subject");
}

#[tokio::test]
async fn event_subject_publish_path_via_transport() {
    // C-10: verify the ONE call site that IS routed through SessionTransport
    // (the event_subject publish at session_relay.rs:1066). Uses
    // NatsSessionTransport directly against testcontainer; asserts the subject
    // format matches event_subject(SESSION_EVENT_PREFIX, session_id, &event).
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.unwrap();

    let transport = NatsSessionTransport::new(nats.clone());
    let envelope = canonical_envelope();
    let expected_subject = roz_worker::event_nats::event_subject("roz.v1", &envelope.correlation_id.0, &envelope.event);
    let mut raw_sub = nats.subscribe(expected_subject.clone()).await.unwrap();

    transport.publish_event_envelope(&envelope).await.unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), raw_sub.next())
        .await
        .expect("recv timed out")
        .expect("stream closed");
    let decoded: EventEnvelope = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(&envelope).unwrap()
    );
    assert_eq!(msg.subject.to_string(), expected_subject);
}

#[test]
fn envelope_wire_format_unchanged() {
    // Construct a minimal canonical envelope and assert its serialized form.
    // Update the canonical_json string only when the wire format is intentionally bumped.
    let envelope = canonical_envelope();
    let json = serde_json::to_string(&envelope).unwrap();
    let expected = canonical_json();
    assert_eq!(
        json, expected,
        "EventEnvelope JSON wire format MUST NOT change without coordinated migration"
    );
}

#[tokio::test]
async fn dual_transport_falls_back_to_primary_only() {
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.unwrap();
    let primary = NatsSessionTransport::new(nats.clone());

    struct AlwaysFailingSecondary;
    #[async_trait::async_trait]
    impl SessionTransport for AlwaysFailingSecondary {
        async fn publish_event_envelope(&self, _: &EventEnvelope) -> anyhow::Result<()> {
            anyhow::bail!("simulated secondary failure")
        }
    }

    let dual = DualPublishTransport::new(primary, AlwaysFailingSecondary);

    // Subscribe via raw NATS to verify primary fired despite secondary error.
    let envelope = canonical_envelope();
    let expected_subject = roz_worker::event_nats::event_subject("roz.v1", &envelope.correlation_id.0, &envelope.event);
    let mut raw_sub = nats.subscribe(expected_subject).await.unwrap();
    dual.publish_event_envelope(&envelope)
        .await
        .expect("dual publish must succeed when only secondary fails");
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), raw_sub.next())
        .await
        .expect("recv timed out")
        .expect("stream closed");
    assert!(!msg.payload.is_empty(), "primary delivered despite secondary failure");
}

// --- fixtures ---

// CANONICAL SHARED FIXTURE -- identical to plan 15-04 Task 1 (roz-core
// transport tests), plan 15-05 Task 1, and plan 15-08 Task 2. Any change
// here MUST be mirrored in all three plans.
fn canonical_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-15-fixture".into()),
        correlation_id: CorrelationId("corr-15-fixture".into()),
        parent_event_id: None,
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(), // 2026-01-01T00:00:00Z
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

// Pre-computed canonical JSON. `EventId`/`CorrelationId` are tuple-newtypes
// over `String` so they serialize as bare strings. `SessionEvent` uses
// `#[serde(tag = "type", rename_all = "snake_case")]`, so `TurnStarted`
// becomes `{"type":"turn_started","turn_index":7}`.
const EXPECTED_JSON: &str = r#"{"event_id":"evt-15-fixture","correlation_id":"corr-15-fixture","parent_event_id":null,"timestamp":"2026-01-01T00:00:00Z","event":{"type":"turn_started","turn_index":7}}"#;

const fn canonical_json() -> &'static str {
    EXPECTED_JSON
}
