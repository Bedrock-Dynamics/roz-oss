//! Edge health monitoring and degradation tracking.

use serde::{Deserialize, Serialize};
use std::time::Duration;

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
