//! Typed publish/subscribe primitives over a `zenoh::Session`.
//!
//! Pre-declared `Publisher`s use `CongestionControl::Drop` per D-16 (freshness
//! over completeness). Subscribers are channel-handler based per D-14 — a
//! single spawned task owns the `flume::Receiver`, decodes per-sample
//! (`serde_json` per D-13), and fans decoded values out via
//! `tokio::sync::broadcast::Sender<T>` for downstream consumers.

use serde::de::DeserializeOwned;
use zenoh::Session;
use zenoh::pubsub::Publisher;
use zenoh::qos::{CongestionControl, Priority};
use zenoh::sample::Sample;

/// Declare a `Publisher` with `CongestionControl::Drop` and `Priority::Data`.
///
/// # Errors
/// Wraps any zenoh declare error with the requested key for diagnostics.
pub async fn declare_drop_publisher(session: &Session, key: impl Into<String>) -> anyhow::Result<Publisher<'static>> {
    let key = key.into();
    session
        .declare_publisher(key.clone())
        .congestion_control(CongestionControl::Drop)
        .priority(Priority::Data)
        .await
        .map_err(|e| anyhow::anyhow!("declare_publisher({key}) failed: {e}"))
}

/// Spawn a per-topic subscriber task that fans samples out via broadcast.
///
/// Decode failures emit `tracing::warn!` with the topic label + error and
/// drop the sample. Fatal subscriber errors (session closed) emit
/// `tracing::error!` and terminate the task; the broadcast sender is dropped,
/// signaling EOF to all receivers.
///
/// # Errors
/// Returns the underlying `declare_subscriber` failure synchronously.
pub async fn spawn_topic_fanout<T>(
    session: Session,
    key_expr: String,
    topic_label: &'static str,
    capacity: usize,
) -> anyhow::Result<tokio::sync::broadcast::Sender<T>>
where
    T: DeserializeOwned + Clone + Send + 'static,
{
    let (tx, _rx) = tokio::sync::broadcast::channel::<T>(capacity);
    let tx_task = tx.clone();
    let sub = session
        .declare_subscriber(&key_expr)
        .with(flume::bounded::<Sample>(64))
        .await
        .map_err(|e| anyhow::anyhow!("declare_subscriber({key_expr}) failed: {e}"))?;
    tokio::spawn(async move {
        loop {
            match sub.recv_async().await {
                Ok(sample) => {
                    let bytes = sample.payload().to_bytes();
                    match serde_json::from_slice::<T>(&bytes) {
                        Ok(value) => {
                            // broadcast::send returns Err only if no receivers; that's fine.
                            let _ = tx_task.send(value);
                        }
                        Err(e) => tracing::warn!(
                            topic = topic_label,
                            error = %e,
                            "decode failed; dropping sample",
                        ),
                    }
                }
                Err(e) => {
                    tracing::error!(
                        topic = topic_label,
                        error = %e,
                        "subscriber terminated",
                    );
                    break;
                }
            }
        }
    });
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_only_config() -> zenoh::Config {
        // multicast disabled; loopback session only — for in-process smoke.
        let cfg = r#"{
          mode: "peer",
          scouting: { multicast: { enabled: false } },
          listen: { endpoints: [] },
          connect: { endpoints: [] },
        }"#;
        zenoh::Config::from_json5(cfg).expect("valid")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn declare_drop_publisher_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let _pub = declare_drop_publisher(&session, "test/pub").await.expect("declare ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn spawn_topic_fanout_returns_sender() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let tx = spawn_topic_fanout::<serde_json::Value>(session, "test/fanout".into(), "test", 8)
            .await
            .expect("spawn ok");
        let mut rx = tx.subscribe();
        // smoke: receiver exists and isn't immediately closed
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
