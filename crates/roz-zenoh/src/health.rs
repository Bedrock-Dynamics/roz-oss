//! Transport health monitoring for the Zenoh edge layer.
//!
//! Two layers coexist here:
//!
//! - `TransportHealthMonitor` (legacy): a synchronous struct that tracks one
//!   heartbeat timestamp and reports [`EdgeTransportHealth`] on demand. Kept
//!   for its in-process call sites; do not extend.
//! - `spawn_heartbeat_publisher` / `spawn_liveliness_monitor` /
//!   `spawn_subsystem_freshness_monitor` (plan 15-06): three async tasks that
//!   fold peer-presence and subsystem freshness into an
//!   [`EdgeHealthAggregator`] watch channel and publish the rolled-up health
//!   onto `roz/<robot_id>/transport/health` per D-23/D-24.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use roz_core::edge_health::{EdgeHealthHandle, EdgeHealthSignal, EdgeTransportHealth};
use tokio::sync::watch;
use zenoh::Session;

use crate::pubsub::declare_drop_publisher;

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

/// Default heartbeat cadence (D-24: 2s publish, 10s staleness window).
pub const HEARTBEAT_CADENCE: Duration = Duration::from_secs(2);
/// Staleness window before a peer's heartbeat is considered degraded (D-24).
pub const HEARTBEAT_STALENESS: Duration = Duration::from_secs(10);
/// Subsystem-freshness window — if an edge-state-bus topic sees no sample in
/// this window the subsystem is marked stale (D-23).
pub const SUBSYSTEM_FRESHNESS: Duration = Duration::from_secs(15);

/// Publish the worker's own health on `roz/<robot_id>/transport/health`.
///
/// Publishes the current [`EdgeTransportHealth`] every `cadence` AND
/// immediately on every state transition observed via the watch channel.
/// Uses `CongestionControl::Drop` via `declare_drop_publisher` per D-16.
///
/// # Errors
/// Returns the publisher declare error synchronously; the spawned loop's
/// runtime errors are logged and terminate the task.
pub async fn spawn_heartbeat_publisher(
    session: Session,
    robot_id: String,
    mut health_rx: watch::Receiver<EdgeTransportHealth>,
    cadence: Duration,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let key = format!("roz/{robot_id}/transport/health");
    let publisher = declare_drop_publisher(&session, key).await?;
    Ok(tokio::spawn(async move {
        let mut tick = tokio::time::interval(cadence);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let health = health_rx.borrow().clone();
                    match serde_json::to_vec(&health) {
                        Ok(bytes) => {
                            if let Err(e) = publisher.put(bytes).await {
                                tracing::warn!(error = %e, "heartbeat publish failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "heartbeat encode failed"),
                    }
                }
                changed = health_rx.changed() => {
                    if changed.is_err() {
                        // All senders dropped — aggregator shut down; exit cleanly.
                        break;
                    }
                    let health = health_rx.borrow_and_update().clone();
                    match serde_json::to_vec(&health) {
                        Ok(bytes) => {
                            if let Err(e) = publisher.put(bytes).await {
                                tracing::warn!(error = %e, "heartbeat transition publish failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "heartbeat transition encode failed"),
                    }
                }
            }
        }
    }))
}

/// Subscribe to `roz/peers/*` liveliness and feed PeerLost/PeerRecovered
/// signals into the [`EdgeHealthAggregator`].
///
/// # Errors
/// Returns the liveliness `declare_subscriber` error synchronously.
pub async fn spawn_liveliness_monitor(
    session: Session,
    handle: EdgeHealthHandle,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let sub = session
        .liveliness()
        .declare_subscriber("roz/peers/*")
        .with(flume::bounded::<zenoh::sample::Sample>(32))
        .await
        .map_err(|e| anyhow::anyhow!("liveliness declare_subscriber failed: {e}"))?;
    Ok(tokio::spawn(async move {
        loop {
            match sub.recv_async().await {
                Ok(sample) => {
                    let key = sample.key_expr().as_str().to_string();
                    // Only accept the single-chunk segment after "roz/peers/";
                    // ignore deeper keys like "roz/peers/<id>/identity" which
                    // zenoh's single-star MAY deliver on some configurations.
                    let Some(robot_id) = key.strip_prefix("roz/peers/") else {
                        continue;
                    };
                    if robot_id.is_empty() || robot_id.contains('/') {
                        continue;
                    }
                    let robot_id = robot_id.to_string();
                    match sample.kind() {
                        zenoh::sample::SampleKind::Put => {
                            handle.send(EdgeHealthSignal::PeerRecovered { robot_id }).await;
                        }
                        zenoh::sample::SampleKind::Delete => {
                            handle.send(EdgeHealthSignal::PeerLost { robot_id }).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "liveliness monitor terminated");
                    break;
                }
            }
        }
    }))
}

/// Monitor subsystem-freshness on a set of wildcard key expressions.
///
/// For each `(name, key_expr)` pair, a subscriber records a last-seen timestamp;
/// a single ticker task scans every 2s and emits
/// [`EdgeHealthSignal::SubsystemStale`] / [`EdgeHealthSignal::SubsystemFresh`]
/// when the `freshness_window` threshold crosses. Never-seen subsystems start
/// stale so a missing publisher is immediately flagged (D-23).
///
/// # Errors
/// Returns the first per-subscriber `declare_subscriber` failure.
pub async fn spawn_subsystem_freshness_monitor<S: std::hash::BuildHasher>(
    session: Session,
    subsystems: HashMap<&'static str, String, S>,
    handle: EdgeHealthHandle,
    freshness_window: Duration,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let last_seen: Arc<Mutex<HashMap<&'static str, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

    for (name, key_expr) in &subsystems {
        let name = *name;
        let last_seen = last_seen.clone();
        let sub = session
            .declare_subscriber(key_expr)
            .with(flume::bounded::<zenoh::sample::Sample>(64))
            .await
            .map_err(|e| anyhow::anyhow!("subsystem freshness subscriber {name} failed: {e}"))?;
        tokio::spawn(async move {
            loop {
                match sub.recv_async().await {
                    Ok(_sample) => {
                        last_seen.lock().insert(name, Instant::now());
                    }
                    Err(e) => {
                        tracing::error!(subsystem = name, error = %e, "freshness subscriber terminated");
                        break;
                    }
                }
            }
        });
    }

    let ticker_names: Vec<&'static str> = subsystems.keys().copied().collect();
    let last_seen_ticker = last_seen.clone();
    Ok(tokio::spawn(async move {
        let mut stale_set: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let now = Instant::now();
            // Snapshot the map under the lock, then release before any await.
            let map = last_seen_ticker.lock().clone();
            for name in &ticker_names {
                let is_stale = map.get(name).is_none_or(|t| now.duration_since(*t) > freshness_window);
                let was_stale = stale_set.contains(name);
                match (is_stale, was_stale) {
                    (true, false) => {
                        stale_set.insert(name);
                        handle
                            .send(EdgeHealthSignal::SubsystemStale {
                                name: (*name).to_string(),
                            })
                            .await;
                    }
                    (false, true) => {
                        stale_set.remove(name);
                        handle
                            .send(EdgeHealthSignal::SubsystemFresh {
                                name: (*name).to_string(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }
        }
    }))
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

    fn peer_only_config() -> zenoh::Config {
        zenoh::Config::from_json5(
            r#"{
          mode: "peer",
          scouting: { multicast: { enabled: false } },
          listen: { endpoints: [] },
          connect: { endpoints: [] },
        }"#,
        )
        .unwrap()
    }

    // Zenoh runtime rejects current_thread scheduler; 15-02 established this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn heartbeat_publisher_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let (_tx, rx) = watch::channel(EdgeTransportHealth::Healthy);
        let handle = spawn_heartbeat_publisher(session, "r-test".into(), rx, Duration::from_millis(100))
            .await
            .expect("spawn ok");
        tokio::time::sleep(Duration::from_millis(250)).await;
        handle.abort();
    }
}
