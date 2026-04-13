//! The edge state bus — typed pub/sub over Zenoh for non-hot-path state.
//!
//! Provides a structured interface for publishing and subscribing to
//! robot-scoped topics on the Zenoh network.

use std::collections::HashMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use zenoh::Session;
use zenoh::pubsub::Publisher;

use crate::pubsub::{declare_drop_publisher, spawn_topic_fanout};
use crate::topics::{
    CONTROLLER_EVIDENCE, PERCEPTION_AVAILABILITY, SAFETY_INTERVENTIONS, TELEMETRY_SUMMARY, TRANSPORT_HEALTH, TopicDef,
};

/// Default per-topic broadcast capacity (D-14 Claude's discretion).
///
/// Chosen to absorb ~1s of bursty samples without blocking when subscribers
/// momentarily lag.
const DEFAULT_BROADCAST_CAPACITY: usize = 64;

/// The 5 edge-state-bus topics pre-declared by `EdgeStateBusRunner` (D-08 + C-05).
///
/// Coordination topics (`COORDINATION_POSE`, `COORDINATION_BARRIER`) are **NOT** in
/// this list. Per CONTEXT D-25/D-26, coordination uses the GLOBAL namespace
/// `roz/coordination/...` (not robot-scoped), and coordination publishers are
/// dynamic per-robot-id / per-barrier (D-15). Coordination publish/subscribe
/// lives in `ZenohCoordinator` (plan 15-07), not here.
pub const ALL_TOPICS: &[&TopicDef] = &[
    &TELEMETRY_SUMMARY,
    &CONTROLLER_EVIDENCE,
    &SAFETY_INTERVENTIONS,
    &PERCEPTION_AVAILABILITY,
    &TRANSPORT_HEALTH,
];

/// The edge state bus — typed pub/sub over Zenoh for non-hot-path state.
///
/// Each bus instance is scoped to a single robot ID. Topic keys are formed as
/// `roz/<robot_id>/<topic_suffix>`, matching the coordination key expressions
/// used throughout the roz Zenoh layer.
pub struct EdgeStateBus {
    robot_id: String,
    // zenoh::Session would go here when the Zenoh session is wired
}

impl EdgeStateBus {
    /// Create a new `EdgeStateBus` scoped to the given robot.
    #[must_use]
    pub fn new(robot_id: &str) -> Self {
        Self {
            robot_id: robot_id.to_string(),
        }
    }

    /// Return the robot ID this bus is scoped to.
    #[must_use]
    pub fn robot_id(&self) -> &str {
        &self.robot_id
    }

    /// Format a full topic key for the given topic definition.
    ///
    /// Key format: `roz/<robot_id>/<topic_suffix>`
    #[must_use]
    pub fn topic_key(&self, topic: &TopicDef) -> String {
        format!("roz/{}/{}", self.robot_id, topic.suffix)
    }
}

/// Runtime adapter that pre-declares one `zenoh::Publisher` per fixed
/// edge-state-bus topic and exposes a typed publish/subscribe API on top of
/// `crate::pubsub` primitives.
///
/// Chose option (a) from the plan: `TopicDef` carries no method accessors in
/// this crate, so runner-internal helpers (`publish_key`, `subscribe_key`)
/// format keys using the same `roz/<robot_id>/<suffix>` pattern as
/// [`EdgeStateBus::topic_key`]. Per-topic memoization is keyed by `topic.suffix`
/// (the stable `&'static str` identifier).
pub struct EdgeStateBusRunner {
    session: Session,
    #[allow(dead_code)] // retained for diagnostics / future key rewrites
    robot_id: String,
    publishers: HashMap<&'static str, Publisher<'static>>,
    /// C-08 memoization: one `broadcast::Sender<T>` per topic suffix, boxed as `Any`
    /// for heterogeneous storage. Downcast to concrete `Sender<T>` on `subscribe()`.
    subscriber_senders: parking_lot::Mutex<HashMap<&'static str, Box<dyn std::any::Any + Send + Sync>>>,
}

impl EdgeStateBusRunner {
    /// Start the runner: pre-declare a publisher for each of the 5 fixed
    /// edge-state-bus topics keyed by `roz/<robot_id>/<topic.suffix>`.
    ///
    /// Coordination topics are excluded per C-05 (see [`ALL_TOPICS`] doc).
    ///
    /// # Errors
    /// Returns the first per-topic declare error encountered.
    pub async fn start(session: Session, robot_id: impl Into<String>) -> anyhow::Result<Self> {
        let robot_id = robot_id.into();
        let mut publishers = HashMap::with_capacity(ALL_TOPICS.len());
        for topic in ALL_TOPICS {
            let key = format!("roz/{}/{}", robot_id, topic.suffix);
            let publisher = declare_drop_publisher(&session, key).await?;
            publishers.insert(topic.suffix, publisher);
        }
        Ok(Self {
            session,
            robot_id,
            publishers,
            subscriber_senders: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Publish a typed payload to the named topic.
    ///
    /// # Errors
    /// Returns serialization or zenoh put failure. Returns an error if `topic`
    /// is not one of the pre-declared [`ALL_TOPICS`] (e.g. a coordination topic).
    pub async fn publish<T: Serialize + Sync>(&self, topic: &'static TopicDef, value: &T) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(|e| anyhow::anyhow!("encode {} failed: {e}", topic.suffix))?;
        let pubr = self
            .publishers
            .get(topic.suffix)
            .ok_or_else(|| anyhow::anyhow!("publisher missing for topic {}", topic.suffix))?;
        pubr.put(bytes)
            .await
            .map_err(|e| anyhow::anyhow!("publish {} failed: {e}", topic.suffix))
    }

    /// Subscribe to the wildcard subscribe-key of `topic` (peers across all `robot_id`s).
    /// Returns a `broadcast::Receiver<T>` per CONTEXT D-14 (C-08 fix).
    ///
    /// **Memoization (C-08 fix):** One spawned fanout task per topic suffix. First
    /// `subscribe()` call for a topic creates the `broadcast::Sender` + spawns the
    /// decoder task; subsequent calls return new `Receiver`s from the existing
    /// `Sender`. This prevents N parallel Zenoh subscribers for the same key.
    ///
    /// Subscribe key uses a wildcard `*` in the `robot_id` slot to observe all
    /// peers on the LAN.
    ///
    /// # Errors
    /// Returns subscriber declare failure synchronously; per-sample decode
    /// failures are logged inside the spawned fanout task (warn + drop).
    /// Returns an error if a prior `subscribe()` call for the same topic
    /// used a different `T` (Any downcast mismatch).
    pub async fn subscribe<T>(&self, topic: &'static TopicDef) -> anyhow::Result<tokio::sync::broadcast::Receiver<T>>
    where
        T: DeserializeOwned + Clone + Send + 'static,
    {
        let key_id = topic.suffix;
        {
            let guard = self.subscriber_senders.lock();
            if let Some(entry) = guard.get(key_id) {
                if let Some(sender) = entry.downcast_ref::<tokio::sync::broadcast::Sender<T>>() {
                    return Ok(sender.subscribe());
                }
                anyhow::bail!(
                    "EdgeStateBus::subscribe::<T> type mismatch for topic {key_id}: a different T was registered previously"
                );
            }
        }
        let sub_key = format!("roz/*/{}", topic.suffix);
        let sender =
            spawn_topic_fanout::<T>(self.session.clone(), sub_key, topic.suffix, DEFAULT_BROADCAST_CAPACITY).await?;
        let rx = sender.subscribe();
        // Race: if another task inserted in between, prefer the existing entry.
        let mut guard = self.subscriber_senders.lock();
        if let Some(entry) = guard.get(key_id)
            && let Some(existing) = entry.downcast_ref::<tokio::sync::broadcast::Sender<T>>()
        {
            let existing_rx = existing.subscribe();
            drop(guard);
            return Ok(existing_rx);
        }
        guard.insert(key_id, Box::new(sender));
        drop(guard);
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topics;

    #[test]
    fn topic_key_telemetry_summary() {
        let bus = EdgeStateBus::new("robot-1");
        assert_eq!(
            bus.topic_key(&topics::TELEMETRY_SUMMARY),
            "roz/robot-1/telemetry/summary"
        );
    }

    #[test]
    fn topic_key_transport_health() {
        let bus = EdgeStateBus::new("arm-7");
        assert_eq!(bus.topic_key(&topics::TRANSPORT_HEALTH), "roz/arm-7/transport/health");
    }

    #[test]
    #[allow(deprecated)] // backwards-compat key-expr formatting test
    fn topic_key_coordination_pose() {
        let bus = EdgeStateBus::new("mobile-base");
        assert_eq!(
            bus.topic_key(&topics::COORDINATION_POSE),
            "roz/mobile-base/coordination/pose"
        );
    }

    #[test]
    #[allow(deprecated)] // backwards-compat key-expr formatting test
    fn topic_key_all_topics() {
        let bus = EdgeStateBus::new("r");
        let all_topics = [
            (&topics::TELEMETRY_SUMMARY, "telemetry/summary"),
            (&topics::CONTROLLER_EVIDENCE, "controller/evidence"),
            (&topics::SAFETY_INTERVENTIONS, "safety/interventions"),
            (&topics::PERCEPTION_AVAILABILITY, "perception/availability"),
            (&topics::TRANSPORT_HEALTH, "transport/health"),
            (&topics::COORDINATION_POSE, "coordination/pose"),
            (&topics::COORDINATION_BARRIER, "coordination/barrier"),
        ];
        for (topic, suffix) in all_topics {
            assert_eq!(bus.topic_key(topic), format!("roz/r/{suffix}"));
        }
    }

    #[test]
    fn robot_id_accessor() {
        let bus = EdgeStateBus::new("test-robot");
        assert_eq!(bus.robot_id(), "test-robot");
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;

    fn peer_only_config() -> zenoh::Config {
        let cfg = r#"{
          mode: "peer",
          scouting: { multicast: { enabled: false } },
          listen: { endpoints: [] },
          connect: { endpoints: [] },
        }"#;
        zenoh::Config::from_json5(cfg).expect("valid")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn start_declares_five_edge_state_bus_publishers() {
        // C-05: coordination topics are NOT in ALL_TOPICS; they live in plan 15-07.
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let runner = EdgeStateBusRunner::start(session, "robot-1").await.expect("start");
        assert_eq!(runner.publishers.len(), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn publish_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let runner = EdgeStateBusRunner::start(session, "robot-1").await.unwrap();
        runner
            .publish(&TELEMETRY_SUMMARY, &serde_json::json!({"x": 1}))
            .await
            .expect("publish ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn subscribe_returns_broadcast_receiver() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let runner = EdgeStateBusRunner::start(session, "robot-1").await.unwrap();
        let _rx: tokio::sync::broadcast::Receiver<serde_json::Value> = runner
            .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
            .await
            .expect("sub ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn subscribe_memoizes_one_task_per_topic() {
        // C-08: two subscribe() calls for same topic must share ONE spawned fanout task
        // (i.e. only one broadcast::Sender exists in the memo map).
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let runner = EdgeStateBusRunner::start(session, "robot-1").await.unwrap();
        let _rx1 = runner
            .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
            .await
            .expect("sub ok 1");
        let _rx2 = runner
            .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
            .await
            .expect("sub ok 2");
        assert_eq!(
            runner.subscriber_senders.lock().len(),
            1,
            "exactly one fanout task memoized"
        );
    }
}
