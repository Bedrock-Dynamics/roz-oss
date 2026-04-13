//! NATS implementation of `roz_core::transport::SessionTransport` (C-01 narrowed).
//!
//! This impl models ONLY the worker-originated `EventEnvelope` publish seam at
//! `session_relay.rs:1066` (the `event_subject(...)` publish). All other NATS
//! operations -- wildcard subscribe, per-session subscribe, `runtime_checkpoint`,
//! canonical response publish -- remain INLINE in `session_relay.rs` unchanged.
//!
//! Preserves the existing NATS subject format (`event_subject(prefix, session_id, event)`)
//! and JSON envelope wire format byte-for-byte (D-18 BLOCKING regression contract).
//!
//! `NatsSessionTransport` derives `session_id` from `envelope.correlation_id.0`,
//! which matches the current call-site behavior: each edge session uses its
//! `session_id` as the `correlation_id` for all envelopes it publishes.

use async_nats::Client;
use async_trait::async_trait;
use roz_core::session::event::EventEnvelope;
use roz_core::transport::SessionTransport;

use crate::event_nats::event_subject;
use crate::session_relay::SESSION_EVENT_PREFIX;

/// Publishes `EventEnvelope`s to NATS on the `event_subject` seam only.
///
/// Constructed at the worker boundary with the same `async_nats::Client` that
/// `session_relay` uses for its inline NATS ops; the two share no state, so
/// routing remains a pure function of the envelope plus the pinned prefix.
pub struct NatsSessionTransport {
    nats: Client,
}

impl NatsSessionTransport {
    #[must_use]
    pub const fn new(nats: Client) -> Self {
        Self { nats }
    }
}

#[async_trait]
impl SessionTransport for NatsSessionTransport {
    async fn publish_event_envelope(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        // Matches session_relay.rs:1065-1067 exactly:
        //   let payload = serde_json::to_vec(envelope)?;
        //   let event_subject = event_subject(SESSION_EVENT_PREFIX, session_id, &envelope.event);
        //   nats.publish(event_subject, payload.into()).await?;
        //
        // `session_id` comes from `envelope.correlation_id.0` -- the relay uses
        // the session_id as the correlation_id when constructing envelopes
        // (see `publish_session_event` in session_relay.rs).
        let session_id = envelope.correlation_id.0.as_str();
        let subject = event_subject(SESSION_EVENT_PREFIX, session_id, &envelope.event);
        let payload = serde_json::to_vec(envelope)?;
        self.nats
            .publish(subject, payload.into())
            .await
            .map_err(|e| anyhow::anyhow!("nats publish_event_envelope failed: {e}"))?;
        Ok(())
    }
}
