//! Local multi-robot coordination primitives via Zenoh P2P.
//!
//! Provides shared pose broadcasting and barrier synchronization on the
//! GLOBAL `roz/coordination/...` namespace (D-25). This is NOT routed
//! through the robot-scoped edge-state-bus runner because coordination is
//! cross-robot by design and must not be prefixed with `roz/<robot_id>/`.

use std::collections::BTreeSet;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use zenoh::Session;
use zenoh::liveliness::LivelinessToken;
use zenoh::qos::{CongestionControl, Priority};

/// A peer identifier in a multi-robot coordination session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerId(pub String);

/// Shared robot pose for co-located coordination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RobotPose {
    /// Unique identifier for this robot.
    pub robot_id: String,
    /// Position as `[x, y, z]` in metres.
    pub position: [f64; 3],
    /// Orientation as quaternion `[w, x, y, z]`.
    pub orientation: [f64; 4],
    /// Timestamp in nanoseconds since epoch.
    pub timestamp_ns: u64,
}

/// Coordinator for local multi-robot sync via Zenoh.
pub struct ZenohCoordinator {
    robot_id: String,
}

/// Current barrier participants, shared between the Queryable-serving task and
/// the liveliness subscriber that maintains it.
pub type ParticipantSet = Arc<RwLock<BTreeSet<String>>>;

/// Guard returned by [`ZenohCoordinator::join_barrier`].
///
/// Dropping this guard announces barrier-leave via the underlying
/// [`LivelinessToken`]'s own `Drop` impl.
pub struct BarrierJoinGuard {
    _token: LivelinessToken,
}

impl ZenohCoordinator {
    /// Create a new coordinator for the given robot.
    #[must_use]
    pub fn new(robot_id: &str) -> Self {
        Self {
            robot_id: robot_id.to_string(),
        }
    }

    /// Key expression for this robot's pose.
    #[must_use]
    pub fn pose_key(&self) -> String {
        format!("roz/coordination/pose/{}", self.robot_id)
    }

    /// Key expression for barrier synchronization.
    #[must_use]
    pub fn barrier_key(barrier_name: &str) -> String {
        format!("roz/coordination/barrier/{barrier_name}")
    }

    /// Publish a pose to `roz/coordination/pose/<robot_id>` (D-25).
    ///
    /// Uses [`CongestionControl::Drop`] — freshness over completeness.
    ///
    /// # Errors
    /// Serialization failure or zenoh publish failure.
    pub async fn publish_pose(session: &Session, pose: &RobotPose) -> anyhow::Result<()> {
        let key = format!("roz/coordination/pose/{}", pose.robot_id);
        let bytes = serde_json::to_vec(pose)?;
        session
            .put(&key, bytes)
            .congestion_control(CongestionControl::Drop)
            .priority(Priority::Data)
            .await
            .map_err(|e| anyhow::anyhow!("publish_pose({key}) failed: {e}"))?;
        Ok(())
    }

    /// Subscribe to all peer poses via wildcard `roz/coordination/pose/*`.
    ///
    /// Returns a [`tokio::sync::broadcast::Sender`] backed by a spawned
    /// fanout task (see [`crate::pubsub::spawn_topic_fanout`]).
    ///
    /// # Errors
    /// Subscriber declare failure.
    pub async fn subscribe_poses(session: Session) -> anyhow::Result<tokio::sync::broadcast::Sender<RobotPose>> {
        crate::pubsub::spawn_topic_fanout::<RobotPose>(
            session,
            "roz/coordination/pose/*".to_string(),
            "coordination_pose",
            64,
        )
        .await
    }

    /// Join a barrier by declaring a [`LivelinessToken`] at
    /// `roz/coordination/barrier/<name>/<robot_id>` (D-26).
    ///
    /// The returned [`BarrierJoinGuard`]'s `Drop` announces barrier-leave.
    ///
    /// # Errors
    /// Liveliness token declare failure.
    pub async fn join_barrier(session: &Session, name: &str, robot_id: &str) -> anyhow::Result<BarrierJoinGuard> {
        let key = format!("roz/coordination/barrier/{name}/{robot_id}");
        let token = session
            .liveliness()
            .declare_token(&key)
            .await
            .map_err(|e| anyhow::anyhow!("declare_token({key}) failed: {e}"))?;
        Ok(BarrierJoinGuard { _token: token })
    }

    /// Observe barrier membership.
    ///
    /// Seeds a [`ParticipantSet`] via `liveliness().get()`, then keeps it
    /// fresh with a liveliness subscriber that handles Put/Delete.
    ///
    /// Returns the shared participant set and the [`tokio::task::JoinHandle`]
    /// of the background task that maintains it. The caller should retain
    /// the handle (aborting it on shutdown) to avoid leaking the subscriber.
    ///
    /// # Errors
    /// Liveliness get or `declare_subscriber` failure.
    pub async fn observe_barrier(
        session: Session,
        name: String,
    ) -> anyhow::Result<(ParticipantSet, tokio::task::JoinHandle<()>)> {
        let wildcard = format!("roz/coordination/barrier/{name}/*");
        let participants: ParticipantSet = Arc::new(RwLock::new(BTreeSet::new()));

        // Seed current members (late-joiner pattern).
        let existing = session
            .liveliness()
            .get(&wildcard)
            .await
            .map_err(|e| anyhow::anyhow!("observe_barrier liveliness get failed: {e}"))?;
        while let Ok(reply) = existing.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            if let Some(robot_id) = extract_robot_id(sample.key_expr().as_str(), &name) {
                participants.write().insert(robot_id);
            }
        }

        // Subscribe to future changes.
        let sub = session
            .liveliness()
            .declare_subscriber(&wildcard)
            .with(flume::bounded::<zenoh::sample::Sample>(32))
            .await
            .map_err(|e| anyhow::anyhow!("observe_barrier declare_subscriber failed: {e}"))?;
        let participants_task = participants.clone();
        let name_task = name.clone();
        let task = tokio::spawn(async move {
            loop {
                match sub.recv_async().await {
                    Ok(sample) => {
                        let Some(robot_id) = extract_robot_id(sample.key_expr().as_str(), &name_task) else {
                            continue;
                        };
                        match sample.kind() {
                            zenoh::sample::SampleKind::Put => {
                                participants_task.write().insert(robot_id);
                            }
                            zenoh::sample::SampleKind::Delete => {
                                participants_task.write().remove(&robot_id);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, barrier = %name_task, "barrier subscriber terminated");
                        break;
                    }
                }
            }
        });
        Ok((participants, task))
    }

    /// Declare a Queryable on `roz/coordination/barrier/<name>` that serves
    /// late-joiner queries with the current participant set as JSON
    /// `Vec<String>` (D-26).
    ///
    /// Returns the [`tokio::task::JoinHandle`] of the background task that
    /// handles incoming queries; the caller retains ownership.
    ///
    /// # Errors
    /// Queryable declare failure.
    pub async fn declare_barrier_queryable(
        session: Session,
        name: String,
        participants: ParticipantSet,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let key = format!("roz/coordination/barrier/{name}");
        let queryable = session
            .declare_queryable(&key)
            .await
            .map_err(|e| anyhow::anyhow!("declare_queryable({key}) failed: {e}"))?;
        let key_for_task = key.clone();
        Ok(tokio::spawn(async move {
            loop {
                match queryable.recv_async().await {
                    Ok(query) => {
                        let members: Vec<String> = participants.read().iter().cloned().collect();
                        match serde_json::to_vec(&members) {
                            Ok(bytes) => {
                                let reply_ke = query.key_expr().clone();
                                if let Err(e) = query.reply(reply_ke, bytes).await {
                                    tracing::warn!(
                                        error = %e,
                                        barrier = %key_for_task,
                                        "barrier queryable reply failed",
                                    );
                                }
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                barrier = %key_for_task,
                                "barrier participants encode failed",
                            ),
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            barrier = %key_for_task,
                            "barrier queryable terminated",
                        );
                        break;
                    }
                }
            }
        }))
    }

    /// Query the current participant set for a named barrier (D-26).
    ///
    /// Fetches the snapshot via `session.get` without waiting for liveliness
    /// events — useful for late joiners.
    ///
    /// # Errors
    /// `session.get` failure, no reply, or JSON decode failure.
    pub async fn query_barrier_participants(session: &Session, name: &str) -> anyhow::Result<Vec<String>> {
        let key = format!("roz/coordination/barrier/{name}");
        let replies = session
            .get(&key)
            .await
            .map_err(|e| anyhow::anyhow!("query_barrier_participants get({key}) failed: {e}"))?;
        match replies.recv_async().await {
            Ok(reply) => {
                let sample = reply
                    .result()
                    .map_err(|e| anyhow::anyhow!("barrier query reply was error: {e:?}"))?;
                let bytes = sample.payload().to_bytes();
                let participants: Vec<String> = serde_json::from_slice(&bytes)?;
                Ok(participants)
            }
            Err(e) => anyhow::bail!("no reply for barrier query {key}: {e}"),
        }
    }
}

/// Extract `<robot_id>` from `roz/coordination/barrier/<barrier_name>/<robot_id>`.
fn extract_robot_id(key: &str, barrier_name: &str) -> Option<String> {
    let prefix = format!("roz/coordination/barrier/{barrier_name}/");
    key.strip_prefix(&prefix).and_then(|rest| {
        // Reject nested segments (single-chunk wildcard semantics).
        if rest.is_empty() || rest.contains('/') {
            None
        } else {
            Some(rest.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pose_key_format() {
        let coord = ZenohCoordinator::new("robot-42");
        assert_eq!(coord.pose_key(), "roz/coordination/pose/robot-42");
    }

    #[test]
    fn barrier_key_format() {
        assert_eq!(
            ZenohCoordinator::barrier_key("sync-start"),
            "roz/coordination/barrier/sync-start"
        );
    }
}

#[cfg(test)]
mod coordination_api_tests {
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

    #[test]
    fn extract_robot_id_parses_barrier_key() {
        assert_eq!(
            extract_robot_id("roz/coordination/barrier/sync-start/r1", "sync-start"),
            Some("r1".into()),
        );
        assert_eq!(extract_robot_id("other/key", "sync-start"), None);
        // Nested segments rejected.
        assert_eq!(
            extract_robot_id("roz/coordination/barrier/sync-start/r1/extra", "sync-start"),
            None,
        );
        // Empty robot_id rejected.
        assert_eq!(
            extract_robot_id("roz/coordination/barrier/sync-start/", "sync-start"),
            None,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn publish_pose_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let pose = RobotPose {
            robot_id: "r1".into(),
            position: [0.0; 3],
            orientation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 0,
        };
        ZenohCoordinator::publish_pose(&session, &pose)
            .await
            .expect("publish ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn subscribe_poses_returns_sender() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let tx = ZenohCoordinator::subscribe_poses(session).await.expect("sub ok");
        let mut rx = tx.subscribe();
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn join_barrier_guard_drop_smoke() {
        let session = zenoh::open(peer_only_config()).await.unwrap();
        let guard = ZenohCoordinator::join_barrier(&session, "sync-start", "r1")
            .await
            .expect("join ok");
        drop(guard);
    }
}
