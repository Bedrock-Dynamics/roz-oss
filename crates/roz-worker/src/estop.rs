//! E-stop event handling for the edge worker.
//!
//! Subscribes to `safety.estop.{worker_id}` on NATS. On receipt,
//! signals the worker to halt via a `tokio::sync::watch` channel.
//!
//! Safety-critical design: if the NATS subscription drops (connection
//! lost), this is treated AS an e-stop — fail-safe behavior.

/// Subscribe to e-stop events for this worker.
pub async fn subscribe_estop(nats: &async_nats::Client, worker_id: &str) -> anyhow::Result<async_nats::Subscriber> {
    let subject = format!("safety.estop.{worker_id}");
    let sub = nats.subscribe(subject).await?;
    Ok(sub)
}

/// Spawn a task that listens for e-stop events and signals halt.
///
/// Returns a `watch::Receiver<bool>` — when true, the worker must halt.
/// If the NATS subscription drops, treats it as e-stop (fail-safe).
pub fn spawn_estop_listener(mut sub: async_nats::Subscriber) -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        use futures::StreamExt;
        while let Some(_msg) = sub.next().await {
            tracing::error!("E-STOP received — signaling worker halt");
            let _ = tx.send(true);
        }
        // Subscription closed = NATS disconnect. Fail-safe: treat as e-stop.
        tracing::warn!("E-stop subscription lost (NATS disconnect) — treating as e-stop");
        let _ = tx.send(true);
    });
    rx
}

#[cfg(test)]
mod tests {
    #[test]
    fn estop_receiver_starts_false() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        assert!(!*rx.borrow(), "should start as non-halted");
        tx.send(true).unwrap();
        assert!(*rx.borrow(), "should be halted after send");
    }
}
