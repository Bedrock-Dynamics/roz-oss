//! Typed publish/subscribe primitives over a `zenoh::Session`.
//!
//! Pre-declared `Publisher`s use `CongestionControl::Drop` per D-16 (freshness
//! over completeness). Subscribers are channel-handler based per D-14 — a
//! single spawned task owns the `flume::Receiver`, decodes per-sample
//! (`serde_json` per D-13), and fans decoded values out via
//! `tokio::sync::broadcast::Sender<T>` for downstream consumers.

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

    #[tokio::test]
    async fn declare_drop_publisher_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let _pub = declare_drop_publisher(&session, "test/pub")
            .await
            .expect("declare ok");
    }

    #[tokio::test]
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
