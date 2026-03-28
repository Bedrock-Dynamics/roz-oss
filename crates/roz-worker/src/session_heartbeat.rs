use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Publishes session heartbeats to NATS at a fixed interval.
pub async fn run_session_heartbeat(
    nats: async_nats::Client,
    worker_id: String,
    session_id: String,
    cancel: CancellationToken,
) {
    let subject = format!("heartbeat.{worker_id}.{session_id}");
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = nats.publish(
                    subject.clone(),
                    bytes::Bytes::from_static(b"{}"),
                ).await {
                    tracing::warn!(error = %e, session_id, "session heartbeat publish failed");
                }
            }
            () = cancel.cancelled() => {
                tracing::debug!(session_id, "session heartbeat stopped");
                return;
            }
        }
    }
}
