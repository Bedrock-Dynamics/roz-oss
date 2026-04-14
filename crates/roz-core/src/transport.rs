//! Transport-agnostic session-event PUBLISH seam (C-01 narrowed trait).
//!
//! Per review C-01: the trait models ONLY the worker-originated `EventEnvelope`
//! publish point at `crates/roz-worker/src/session_relay.rs:1061`. All other
//! NATS operations (wildcard subscribe, per-session subscribe, `runtime_checkpoint`,
//! canonical response publish) stay INLINE in `session_relay.rs` using the
//! existing `async_nats::Client` directly -- they are NOT abstracted.
//!
//! Why narrow: expanding the trait to model receive/wildcard/checkpoint would
//! either drop Phase 13 behavior or force unplanned trait expansion mid-execution.
//! D-19 ONLY requires dual-publishing worker-originated events; keep the
//! abstraction at exactly that surface.
//!
//! Implementations:
//! - `roz_worker::transport_nats::NatsSessionTransport` -- publishes to
//!   `event_subject(...)` preserving Phase 13 byte format (D-18).
//! - `roz_zenoh::session::ZenohSessionTransport` -- peer-to-peer edge transport
//!   added in Phase 15 (D-19, D-22).
//!
//! `DualPublishTransport` composes two implementations so the worker can
//! publish to both NATS (cloud) and Zenoh (edge peers) concurrently per D-19.

use async_trait::async_trait;

use crate::session::event::EventEnvelope;

/// Transport-agnostic `EventEnvelope` publish seam. Single method per C-01.
///
/// Object-safe: relay code uses `Box<dyn SessionTransport>`.
#[async_trait]
pub trait SessionTransport: Send + Sync + 'static {
    /// Publish a worker-originated `EventEnvelope` to this transport's peer
    /// audience. Key expression / subject / topic derivation is the
    /// implementation's responsibility (NATS uses `event_subject(...)`, Zenoh
    /// uses `roz/sessions/<team_id>/<session_id>`).
    ///
    /// Transport-specific routing context (`tenant_id`, `team_id`, `session_id`) is
    /// embedded IN the envelope (`envelope.event`, `envelope.correlation_id`,
    /// etc.) -- the trait does NOT take them as separate parameters. This keeps
    /// the trait minimal and matches the call site in `session_relay.rs:1061`.
    ///
    /// # Errors
    /// Returns transport-specific publish failure. Caller decides whether to
    /// retry, drop the event, or escalate.
    async fn publish_event_envelope(&self, envelope: &EventEnvelope) -> anyhow::Result<()>;
}

/// Compose two `SessionTransport`s as primary + best-effort secondary (D-19).
///
/// `publish_event_envelope` drives primary and secondary publishes concurrently
/// via `tokio::join!`. A slow or hung secondary does NOT block the primary's
/// progress, and only primary failure propagates; secondary errors are logged
/// at `warn` level.
pub struct DualPublishTransport<P: SessionTransport, S: SessionTransport> {
    primary: P,
    secondary: S,
}

impl<P: SessionTransport, S: SessionTransport> DualPublishTransport<P, S> {
    pub const fn new(primary: P, secondary: S) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl<P: SessionTransport, S: SessionTransport> SessionTransport for DualPublishTransport<P, S> {
    async fn publish_event_envelope(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        // Drive both publishes concurrently so a slow/hung secondary cannot
        // block or delay the primary. Only the primary's result propagates;
        // secondary errors are logged and dropped.
        let primary_fut = self.primary.publish_event_envelope(envelope);
        let secondary_fut = self.secondary.publish_event_envelope(envelope);
        let (primary_res, secondary_res) = tokio::join!(primary_fut, secondary_fut);

        if let Err(e) = secondary_res {
            tracing::warn!(
                event_id = %envelope.event_id.0,
                error = %e,
                "secondary session transport publish failed; primary result unaffected",
            );
            // Counter increment for EdgeHealthAggregator wiring lands in plan 15-06.
        }
        primary_res
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::DateTime;

    use super::*;
    use crate::session::event::{CorrelationId, EventId, SessionEvent};

    struct CountingTransport {
        publishes: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl SessionTransport for CountingTransport {
        async fn publish_event_envelope(&self, _: &EventEnvelope) -> anyhow::Result<()> {
            self.publishes.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("forced failure");
            }
            Ok(())
        }
    }

    // CANONICAL SHARED FIXTURE -- keep byte-identical to the one used by:
    //   - crates/roz-worker/tests/session_transport_regression.rs (plan 15-04 Task 3)
    //   - crates/roz-zenoh/src/envelope.rs tests (plan 15-05 Task 1)
    //   - crates/roz-zenoh/tests/signed_session_relay_integration.rs (plan 15-08 Task 2)
    // Any drift breaks the D-18 wire-format regression lock.
    fn sample_envelope() -> EventEnvelope {
        EventEnvelope {
            event_id: EventId("evt-15-fixture".into()),
            correlation_id: CorrelationId("corr-15-fixture".into()),
            parent_event_id: None,
            timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(), // 2026-01-01T00:00:00Z
            event: SessionEvent::TurnStarted { turn_index: 7 },
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe() {
        let p = Arc::new(AtomicUsize::new(0));
        let _boxed: Box<dyn SessionTransport> = Box::new(CountingTransport {
            publishes: p,
            fail: false,
        });
    }

    #[tokio::test]
    async fn dual_secondary_failure_does_not_propagate() {
        let p_count = Arc::new(AtomicUsize::new(0));
        let s_count = Arc::new(AtomicUsize::new(0));
        let dual = DualPublishTransport::new(
            CountingTransport {
                publishes: p_count.clone(),
                fail: false,
            },
            CountingTransport {
                publishes: s_count.clone(),
                fail: true,
            },
        );
        let env = sample_envelope();
        let res = dual.publish_event_envelope(&env).await;
        assert!(res.is_ok(), "secondary failure must not propagate");
        assert_eq!(p_count.load(Ordering::SeqCst), 1, "primary still fires");
        assert_eq!(s_count.load(Ordering::SeqCst), 1, "secondary attempted");
    }

    #[tokio::test]
    async fn dual_primary_failure_propagates() {
        let p_count = Arc::new(AtomicUsize::new(0));
        let s_count = Arc::new(AtomicUsize::new(0));
        let dual = DualPublishTransport::new(
            CountingTransport {
                publishes: p_count,
                fail: true,
            },
            CountingTransport {
                publishes: s_count,
                fail: false,
            },
        );
        let env = sample_envelope();
        assert!(dual.publish_event_envelope(&env).await.is_err());
    }
}
