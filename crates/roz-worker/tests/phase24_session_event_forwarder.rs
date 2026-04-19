//! Phase 24 gap closure (Plan 24-14 Task 1): integration-prove the
//! mpsc→broadcast forwarder that carries `SessionEvent::SafetyViolation`
//! from the pre-dispatch gate onto the worker-level session event broadcast
//! bus.
//!
//! Prior to 24-14 this forwarder was a ~20-line inline tokio::spawn block
//! inside `execute_task` in `crates/roz-worker/src/main.rs` (plan 24-13
//! Task 3). Its delivery contract (fresh envelope IDs, parent=None, Utc
//! timestamp) was covered only by code inspection — no test drove a real
//! `SessionEvent` through an mpsc and asserted it reappeared on the
//! broadcast side with the required envelope shape.
//!
//! This test closes that gap by:
//! 1. Creating mpsc + broadcast channels.
//! 2. Subscribing to the broadcast BEFORE spawning the forwarder (so no
//!    slow-subscriber race can drop the three frames).
//! 3. Spawning `spawn_session_event_forwarder`.
//! 4. Publishing three distinct `SafetyViolation` events.
//! 5. Dropping the mpsc sender to let the forwarder exit cleanly.
//! 6. Asserting each envelope's `event` matches the expected variant, that
//!    `event_id` values are unique, `parent_event_id` is `None`, and the
//!    forwarder's `JoinHandle` completes.
//!
//! Anti-tautology check was performed by temporarily replacing the
//! forwarder body's `while let Some(event) = mpsc_rx.recv().await` with an
//! immediate `break` and confirming the test failed on the receive timeout
//! before restoring the production code.

use std::collections::HashSet;
use std::time::Duration;

use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_worker::session_event_forwarder::spawn_session_event_forwarder;
use tokio::sync::{broadcast, mpsc};

fn violation(kind: &str) -> SessionEvent {
    SessionEvent::SafetyViolation {
        policy_id: "00000000-0000-0000-0000-000000000001".into(),
        violation_kind: kind.into(),
        enforcement_action: "halt".into(),
        details: serde_json::json!({ "kind": kind }),
    }
}

async fn recv_envelope(sub: &mut broadcast::Receiver<EventEnvelope>) -> EventEnvelope {
    tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("broadcast recv did not complete within 2 s")
        .expect("broadcast channel closed before three frames arrived")
}

#[tokio::test]
async fn forwarder_delivers_session_events_with_fresh_envelope_ids() {
    // mpsc cap 64 matches the production forwarder's back-pressure budget
    // (main.rs:527 pre-refactor; still 64 post-refactor).
    let (mpsc_tx, mpsc_rx) = mpsc::channel::<SessionEvent>(64);
    let (broadcast_tx, mut broadcast_rx) = broadcast::channel::<EventEnvelope>(64);

    // Subscribe BEFORE spawn so the forwarder's first send cannot race the
    // subscriber and drop frames. The broadcast_rx above is the primary
    // subscriber; it was created with `channel(_)` so it is already a live
    // subscriber.

    let handle = spawn_session_event_forwarder(mpsc_rx, broadcast_tx.clone());

    let kinds = ["limit_exceeded", "geofence_breach", "deadman_expired"];
    for k in kinds {
        mpsc_tx
            .send(violation(k))
            .await
            .expect("mpsc send while forwarder is alive");
    }
    // Dropping the sender causes the forwarder to observe `recv()` -> None
    // and exit cleanly.
    drop(mpsc_tx);

    let mut envelopes = Vec::with_capacity(3);
    for _ in 0..3 {
        envelopes.push(recv_envelope(&mut broadcast_rx).await);
    }

    // Assertion 1 — order + content fidelity.
    for (env, expected_kind) in envelopes.iter().zip(kinds.iter()) {
        match &env.event {
            SessionEvent::SafetyViolation { violation_kind, .. } => {
                assert_eq!(
                    violation_kind, expected_kind,
                    "envelope order or payload corrupted: got {violation_kind}, expected {expected_kind}"
                );
            }
            other => panic!("expected SafetyViolation, got {other:?}"),
        }
    }

    // Assertion 2 — unique event_id per envelope (forwarder must mint a
    // fresh EventId, not reuse one).
    let ids: HashSet<_> = envelopes.iter().map(|e| e.event_id.0.clone()).collect();
    assert_eq!(ids.len(), 3, "event_id values must be unique across envelopes");

    // Assertion 3 — parent_event_id is always None (the forwarder is not a
    // response to a parent event).
    for env in &envelopes {
        assert!(
            env.parent_event_id.is_none(),
            "parent_event_id must be None for forwarder-generated envelopes"
        );
    }

    // Assertion 4 — correlation_id values are also unique per envelope
    // (CorrelationId::new mints a fresh UUID, matching the pre-refactor
    // inline block at main.rs:535).
    let corrs: HashSet<_> = envelopes.iter().map(|e| e.correlation_id.0.clone()).collect();
    assert_eq!(
        corrs.len(),
        3,
        "correlation_id values must be unique (fresh UUID per envelope)"
    );

    // Assertion 5 — forwarder JoinHandle exits within 1 s of sender drop.
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("forwarder JoinHandle did not complete within 1 s of sender drop")
        .expect("forwarder task panicked");
}
