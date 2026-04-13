//! Edge health monitoring and degradation tracking.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

/// Health of an edge transport (Zenoh, NATS, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EdgeTransportHealth {
    Healthy,
    Degraded { affected: Vec<String> },
    Disconnected,
}

impl EdgeTransportHealth {
    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// Contract for a Zenoh topic's publication frequency and staleness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicContract {
    pub topic: String,
    pub max_publish_interval_ms: u64,
    pub max_staleness_ms: u64,
}

impl TopicContract {
    #[must_use]
    pub const fn max_publish_interval(&self) -> Duration {
        Duration::from_millis(self.max_publish_interval_ms)
    }

    #[must_use]
    pub const fn max_staleness(&self) -> Duration {
        Duration::from_millis(self.max_staleness_ms)
    }
}

#[cfg(test)]
mod aggregator_tests {
    use super::*;

    #[tokio::test]
    async fn starts_healthy() {
        let (_agg, rx, _h) = EdgeHealthAggregator::new(16);
        assert!(matches!(*rx.borrow(), EdgeTransportHealth::Healthy));
    }

    #[tokio::test]
    async fn peer_lost_then_recovered_roundtrip() {
        let (agg, mut rx, h) = EdgeHealthAggregator::new(16);
        let _task = tokio::spawn(agg.run());

        h.send(EdgeHealthSignal::PeerLost {
            robot_id: "r1".into(),
        })
        .await;
        rx.changed().await.unwrap();
        match &*rx.borrow_and_update() {
            EdgeTransportHealth::Degraded { affected } => assert!(affected.iter().any(|a| a == "peer:r1")),
            other => panic!("expected Degraded, got {other:?}"),
        }

        h.send(EdgeHealthSignal::PeerRecovered {
            robot_id: "r1".into(),
        })
        .await;
        rx.changed().await.unwrap();
        assert!(matches!(*rx.borrow(), EdgeTransportHealth::Healthy));
    }

    #[tokio::test]
    async fn subsystem_stale_appears_in_affected() {
        let (agg, mut rx, h) = EdgeHealthAggregator::new(16);
        let _task = tokio::spawn(agg.run());
        h.send(EdgeHealthSignal::SubsystemStale {
            name: "telemetry_summary".into(),
        })
        .await;
        rx.changed().await.unwrap();
        match &*rx.borrow() {
            EdgeTransportHealth::Degraded { affected } => {
                assert!(affected.iter().any(|a| a == "subsystem:telemetry_summary"));
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_down_wins_over_degraded() {
        let (agg, mut rx, h) = EdgeHealthAggregator::new(16);
        let _task = tokio::spawn(agg.run());
        h.send(EdgeHealthSignal::PeerLost {
            robot_id: "r1".into(),
        })
        .await;
        rx.changed().await.unwrap();
        h.send(EdgeHealthSignal::TransportDown).await;
        rx.changed().await.unwrap();
        assert!(matches!(*rx.borrow(), EdgeTransportHealth::Disconnected));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_health_serde_all_variants() {
        let variants: Vec<EdgeTransportHealth> = vec![
            EdgeTransportHealth::Healthy,
            EdgeTransportHealth::Degraded {
                affected: vec!["perception/camera".into()],
            },
            EdgeTransportHealth::Disconnected,
        ];
        for h in variants {
            let json = serde_json::to_string(&h).unwrap();
            let back: EdgeTransportHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(h, back);
        }
    }

    #[test]
    fn edge_health_is_healthy() {
        assert!(EdgeTransportHealth::Healthy.is_healthy());
        assert!(!EdgeTransportHealth::Degraded { affected: vec![] }.is_healthy());
        assert!(!EdgeTransportHealth::Disconnected.is_healthy());
    }

    #[test]
    fn topic_contract_serde() {
        let tc = TopicContract {
            topic: "roz/robot-1/telemetry/summary".into(),
            max_publish_interval_ms: 100,
            max_staleness_ms: 500,
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: TopicContract = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
        assert_eq!(back.max_publish_interval(), Duration::from_millis(100));
        assert_eq!(back.max_staleness(), Duration::from_millis(500));
    }
}
