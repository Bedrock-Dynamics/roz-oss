//! Transport health monitoring for the Zenoh edge layer.
//!
//! Tracks heartbeat liveness and reports [`EdgeTransportHealth`] based on
//! whether a heartbeat was received within the configured timeout window.

use roz_core::edge_health::EdgeTransportHealth;
use std::time::{Duration, Instant};

/// Monitors transport health by tracking heartbeat liveness.
///
/// Reports [`EdgeTransportHealth::Healthy`] when a heartbeat was received
/// within the timeout window, [`EdgeTransportHealth::Degraded`] when stale,
/// and [`EdgeTransportHealth::Disconnected`] when no heartbeat has ever been
/// received.
pub struct TransportHealthMonitor {
    last_heartbeat: Option<Instant>,
    timeout: Duration,
}

impl TransportHealthMonitor {
    /// Create a new monitor with the given timeout.
    pub const fn new(timeout: Duration) -> Self {
        Self {
            last_heartbeat: None,
            timeout,
        }
    }

    /// Record that a heartbeat was received right now.
    pub fn record_heartbeat(&mut self) {
        self.last_heartbeat = Some(Instant::now());
    }

    /// Check the current transport health based on heartbeat liveness.
    pub fn check(&self) -> EdgeTransportHealth {
        self.last_heartbeat.map_or(EdgeTransportHealth::Disconnected, |last| {
            if last.elapsed() <= self.timeout {
                EdgeTransportHealth::Healthy
            } else {
                EdgeTransportHealth::Degraded {
                    affected: vec!["zenoh_transport".to_string()],
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn disconnected_when_no_heartbeat() {
        let monitor = TransportHealthMonitor::new(Duration::from_millis(100));
        assert_eq!(monitor.check(), EdgeTransportHealth::Disconnected);
    }

    #[test]
    fn healthy_when_heartbeat_recent() {
        let mut monitor = TransportHealthMonitor::new(Duration::from_millis(200));
        monitor.record_heartbeat();
        assert_eq!(monitor.check(), EdgeTransportHealth::Healthy);
    }

    #[test]
    fn degraded_when_heartbeat_stale() {
        let mut monitor = TransportHealthMonitor::new(Duration::from_millis(10));
        monitor.record_heartbeat();
        // Wait long enough for the heartbeat to go stale
        thread::sleep(Duration::from_millis(30));
        let health = monitor.check();
        assert!(
            matches!(health, EdgeTransportHealth::Degraded { .. }),
            "expected Degraded, got {health:?}"
        );
    }

    #[test]
    fn healthy_after_heartbeat_recorded() {
        let mut monitor = TransportHealthMonitor::new(Duration::from_secs(10));
        // No heartbeat yet — disconnected
        assert_eq!(monitor.check(), EdgeTransportHealth::Disconnected);
        // Record one — now healthy
        monitor.record_heartbeat();
        assert_eq!(monitor.check(), EdgeTransportHealth::Healthy);
    }
}
