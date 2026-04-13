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

/// Signals that subsystems send to an [`EdgeHealthAggregator`].
///
/// Subsystems (liveliness monitor, freshness monitor, transport open/close)
/// send these so the aggregator folds the stream into the
/// [`EdgeTransportHealth`] watch value consumed by the heartbeat publisher.
#[derive(Debug, Clone)]
pub enum EdgeHealthSignal {
    /// A peer's liveliness token was deleted — treat as lost until restored.
    PeerLost { robot_id: String },
    /// A peer's liveliness token reappeared.
    PeerRecovered { robot_id: String },
    /// A monitored subsystem has gone stale (no samples within the freshness window).
    SubsystemStale { name: String },
    /// A monitored subsystem received a fresh sample; clear the stale marker.
    SubsystemFresh { name: String },
    /// Secondary transport publish counter exceeded threshold (see plan 15-04).
    SecondaryTransportFailing { label: String },
    /// The primary transport went down — overrides any subsystem/peer state.
    TransportDown,
    /// The primary transport came back up — clears the Disconnected override.
    TransportRestored,
}

/// Handle for subsystems to send [`EdgeHealthSignal`]s to an [`EdgeHealthAggregator`].
#[derive(Debug, Clone)]
pub struct EdgeHealthHandle {
    tx: mpsc::Sender<EdgeHealthSignal>,
}

impl EdgeHealthHandle {
    /// Best-effort send. Bounded channel; on full the signal is dropped —
    /// freshness checks re-run so transient loss is recoverable.
    ///
    /// Kept `async` so callers from async contexts can use the usual
    /// `.await` syntax even though `try_send` is non-blocking today.
    #[expect(
        clippy::unused_async,
        reason = "call sites use `.await` uniformly with other EdgeHealthHandle methods; preserves future-compatibility if a blocking `send` is added."
    )]
    pub async fn send(&self, signal: EdgeHealthSignal) {
        let _ = self.tx.try_send(signal);
    }
}

/// Folds [`EdgeHealthSignal`]s into the current [`EdgeTransportHealth`].
///
/// Owned by one task; subsystems hold clones of [`EdgeHealthHandle`] to send
/// signals and the heartbeat publisher holds a [`watch::Receiver`] to consume
/// state changes.
pub struct EdgeHealthAggregator {
    tx: watch::Sender<EdgeTransportHealth>,
    rx_signals: mpsc::Receiver<EdgeHealthSignal>,
    /// Deterministic ordering so the `Vec<String>` inside `Degraded` is stable.
    affected: BTreeSet<String>,
    transport_down: bool,
}

impl EdgeHealthAggregator {
    /// Create a new aggregator. Returns the aggregator (owned), a
    /// [`watch::Receiver`] the heartbeat publisher clones, and an
    /// [`EdgeHealthHandle`] subsystems clone to send signals.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, watch::Receiver<EdgeTransportHealth>, EdgeHealthHandle) {
        let (tx, rx_watch) = watch::channel(EdgeTransportHealth::Healthy);
        let (tx_sig, rx_sig) = mpsc::channel::<EdgeHealthSignal>(capacity);
        let agg = Self {
            tx,
            rx_signals: rx_sig,
            affected: BTreeSet::new(),
            transport_down: false,
        };
        (agg, rx_watch, EdgeHealthHandle { tx: tx_sig })
    }

    fn recompute(&self) -> EdgeTransportHealth {
        if self.transport_down {
            return EdgeTransportHealth::Disconnected;
        }
        if self.affected.is_empty() {
            return EdgeTransportHealth::Healthy;
        }
        EdgeTransportHealth::Degraded {
            affected: self.affected.iter().cloned().collect(),
        }
    }

    fn apply(&mut self, signal: EdgeHealthSignal) {
        match signal {
            EdgeHealthSignal::PeerLost { robot_id } => {
                self.affected.insert(format!("peer:{robot_id}"));
            }
            EdgeHealthSignal::PeerRecovered { robot_id } => {
                self.affected.remove(&format!("peer:{robot_id}"));
            }
            EdgeHealthSignal::SubsystemStale { name } => {
                self.affected.insert(format!("subsystem:{name}"));
            }
            EdgeHealthSignal::SubsystemFresh { name } => {
                self.affected.remove(&format!("subsystem:{name}"));
            }
            EdgeHealthSignal::SecondaryTransportFailing { label } => {
                self.affected.insert(format!("secondary:{label}"));
            }
            EdgeHealthSignal::TransportDown => {
                self.transport_down = true;
            }
            EdgeHealthSignal::TransportRestored => {
                self.transport_down = false;
            }
        }
    }

    /// Run the aggregator loop. Consumes self; terminates when every
    /// [`EdgeHealthHandle`] has been dropped.
    pub async fn run(mut self) {
        while let Some(sig) = self.rx_signals.recv().await {
            self.apply(sig);
            let new = self.recompute();
            // Equal values still notify receivers via watch::Sender's epoch —
            // acceptable for a 2s-cadence publisher. Suppressing no-op notifies
            // would require send_if_modified here.
            let _ = self.tx.send(new);
        }
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

        h.send(EdgeHealthSignal::PeerLost { robot_id: "r1".into() }).await;
        rx.changed().await.unwrap();
        match &*rx.borrow_and_update() {
            EdgeTransportHealth::Degraded { affected } => assert!(affected.iter().any(|a| a == "peer:r1")),
            other => panic!("expected Degraded, got {other:?}"),
        }

        h.send(EdgeHealthSignal::PeerRecovered { robot_id: "r1".into() }).await;
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
        h.send(EdgeHealthSignal::PeerLost { robot_id: "r1".into() }).await;
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
