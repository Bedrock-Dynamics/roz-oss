//! Gazebo joint command publisher.
//!
//! This module provides [`GazeboJointPublisher`], which advertises one
//! `gz.msgs.Double` publisher per joint and maps a [`CommandFrame`] onto those
//! publishers.  It is gated behind the `gazebo` feature flag.

use roz_core::command::CommandFrame;

// ---------------------------------------------------------------------------
// Topic builders
// ---------------------------------------------------------------------------

/// Build the `/cmd_vel` topic path for a single joint.
///
/// Returns `/world/{world}/model/{model}/joint/{joint}/cmd_vel`.
#[must_use]
pub fn joint_cmd_topic(world: &str, model: &str, joint: &str) -> String {
    format!("/world/{world}/model/{model}/joint/{joint}/cmd_vel")
}

/// Build the `/cmd_pos` topic path for a single joint.
///
/// Returns `/world/{world}/model/{model}/joint/{joint}/cmd_pos`.
#[must_use]
pub fn joint_cmd_pos_topic(world: &str, model: &str, joint: &str) -> String {
    format!("/world/{world}/model/{model}/joint/{joint}/cmd_pos")
}

// ---------------------------------------------------------------------------
// GazeboJointPublisher
// ---------------------------------------------------------------------------

/// Publishes per-joint velocity commands to Gazebo via `gz.msgs.Double`.
///
/// One [`gz_transport_rs::Publisher`] is created per joint during [`create`].
/// Each call to [`send`] publishes the corresponding value from the
/// [`CommandFrame`]; missing values default to `0.0`, extra values
/// are silently ignored.
///
/// [`create`]: GazeboJointPublisher::create
/// [`send`]: GazeboJointPublisher::send
pub struct GazeboJointPublisher {
    publishers: Vec<(String, gz_transport_rs::Publisher<gz_transport_rs::msgs::Double>)>,
    partition: String,
}

impl GazeboJointPublisher {
    /// Create a new publisher, advertising one topic per joint.
    ///
    /// Uses [`gz_transport_rs::Node::advertise`] so the Gazebo discovery network
    /// learns about the new publisher before the first message is sent.
    ///
    /// # Errors
    ///
    /// Returns [`gz_transport_rs::Error`] if the ZMQ socket cannot bind or if
    /// the discovery ADVERTISE fails.
    pub async fn create(
        node: &mut gz_transport_rs::Node,
        world: &str,
        model: &str,
        joints: &[&str],
    ) -> gz_transport_rs::Result<Self> {
        let partition = node.partition();
        let mut publishers = Vec::with_capacity(joints.len());

        for &joint in joints {
            let topic = joint_cmd_topic(world, model, joint);
            let publisher = node
                .advertise::<gz_transport_rs::msgs::Double>(&topic, "gz.msgs.Double")
                .await?;
            publishers.push((topic, publisher));
        }

        Ok(Self { publishers, partition })
    }

    /// Publish a [`CommandFrame`] to all registered joint publishers.
    ///
    /// `frame.values[i]` maps to the i-th joint in the order supplied to
    /// [`create`].  If the frame has fewer values than joints, the remaining
    /// joints receive `0.0`.  Extra values beyond the number of registered
    /// joints are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`gz_transport_rs::Error::ChannelClosed`] if the underlying ZMQ
    /// thread has exited.
    pub fn send(&self, frame: &CommandFrame) -> gz_transport_rs::Result<()> {
        for (i, (_topic, publisher)) in self.publishers.iter().enumerate() {
            let value = frame.values.get(i).copied().unwrap_or(0.0);
            let msg = gz_transport_rs::msgs::Double {
                header: None,
                data: value,
            };
            publisher.publish(&self.partition, &msg)?;
        }
        Ok(())
    }

    /// Number of joints this publisher was created for.
    #[must_use]
    pub const fn joint_count(&self) -> usize {
        self.publishers.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_topic_format() {
        let topic = joint_cmd_topic("default", "ur10", "shoulder_pan");
        assert_eq!(topic, "/world/default/model/ur10/joint/shoulder_pan/cmd_vel");
    }

    #[test]
    fn joint_topic_position_mode() {
        let topic = joint_cmd_pos_topic("default", "ur10", "shoulder_pan");
        assert_eq!(topic, "/world/default/model/ur10/joint/shoulder_pan/cmd_pos");
    }
}
