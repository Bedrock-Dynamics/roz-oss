//! Gazebo sensor bridge: converts Gazebo protobuf pose messages to roz domain types.
//!
//! This module is gated behind the `gazebo` feature flag and depends on the
//! `gz-transport` crate.  It is the canonical entry point for ingesting
//! Gazebo world-pose data into the roz platform.

use roz_core::spatial::EntityState;
use std::collections::HashMap;

/// Convert a [`gz_transport_rs::msgs::PoseV`] message into a [`Vec<EntityState>`].
///
/// Each pose inside the message is converted to one [`EntityState`].
/// The coordinate-frame origin is always tagged as `"world"`.
///
/// # Field mapping
///
/// | Gazebo field          | `EntityState` field  | Notes                              |
/// |-----------------------|----------------------|------------------------------------|
/// | `pose.name`           | `id`                 | Model name from Gazebo             |
/// | `"gazebo_model"`      | `kind`               | Constant                           |
/// | `position.{x,y,z}`   | `position`           | `None` when pose has no position   |
/// | `orientation.{w,x,y,z}` | `orientation`     | roz order is `[w,x,y,z]`          |
/// | `"world"`             | `frame_id`           | Always world frame                 |
/// | —                     | `velocity`           | Always `None` (not in `PoseV`)     |
/// | —                     | `properties`         | Always empty                       |
/// | —                     | `timestamp_ns`       | Always `None`                      |
pub fn poses_to_entities(pose_v: &gz_transport_rs::msgs::PoseV) -> Vec<EntityState> {
    pose_v.pose.iter().map(pose_to_entity).collect()
}

fn pose_to_entity(pose: &gz_transport_rs::msgs::Pose) -> EntityState {
    let position = pose.position.as_ref().map(|v| [v.x, v.y, v.z]);
    // Gazebo quaternion struct fields are x, y, z, w — roz convention is [w, x, y, z].
    let orientation = pose.orientation.as_ref().map(|q| [q.w, q.x, q.y, q.z]);

    EntityState {
        id: pose.name.clone(),
        kind: "gazebo_model".to_owned(),
        position,
        orientation,
        velocity: None,
        properties: HashMap::new(),
        timestamp_ns: None,
        frame_id: "world".to_owned(),
        last_observed_ns: None,
        observation_confidence: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gz_transport_rs::msgs::{Pose, PoseV, Quaternion, Vector3d};

    #[test]
    fn converts_pose_v_to_entity_states() {
        let pose_v = PoseV {
            header: None,
            pose: vec![
                Pose {
                    header: None,
                    name: "robot_arm".into(),
                    id: 1,
                    position: Some(Vector3d {
                        header: None,
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                    }),
                    orientation: Some(Quaternion {
                        header: None,
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                        w: 1.0,
                    }),
                },
                Pose {
                    header: None,
                    name: "camera_mount".into(),
                    id: 2,
                    position: Some(Vector3d {
                        header: None,
                        x: -0.5,
                        y: 0.25,
                        z: 1.8,
                    }),
                    orientation: Some(Quaternion {
                        header: None,
                        x: 0.1,
                        y: 0.2,
                        z: 0.3,
                        w: 0.927,
                    }),
                },
            ],
        };

        let entities = poses_to_entities(&pose_v);

        assert_eq!(entities.len(), 2);

        let arm = &entities[0];
        assert_eq!(arm.id, "robot_arm");
        assert_eq!(arm.kind, "gazebo_model");
        assert_eq!(arm.position, Some([1.0, 2.0, 3.0]));
        // roz convention: [w, x, y, z]
        assert_eq!(arm.orientation, Some([1.0, 0.0, 0.0, 0.0]));
        assert_eq!(arm.frame_id, "world");
        assert!(arm.velocity.is_none());
        assert!(arm.properties.is_empty());
        assert!(arm.timestamp_ns.is_none());
        assert!(arm.last_observed_ns.is_none());
        assert_eq!(arm.observation_confidence, 1.0);

        let cam = &entities[1];
        assert_eq!(cam.id, "camera_mount");
        assert_eq!(cam.kind, "gazebo_model");
        assert_eq!(cam.position, Some([-0.5, 0.25, 1.8]));
        assert_eq!(cam.orientation, Some([0.927, 0.1, 0.2, 0.3]));
        assert_eq!(cam.frame_id, "world");
        assert!(cam.velocity.is_none());
        assert!(cam.properties.is_empty());
        assert!(cam.timestamp_ns.is_none());
        assert!(cam.last_observed_ns.is_none());
        assert_eq!(cam.observation_confidence, 1.0);
    }

    #[test]
    fn handles_pose_without_position() {
        let pose_v = PoseV {
            header: None,
            pose: vec![Pose {
                header: None,
                name: "ghost_model".into(),
                id: 99,
                position: None,
                orientation: None,
            }],
        };

        let entities = poses_to_entities(&pose_v);

        assert_eq!(entities.len(), 1);
        let entity = &entities[0];
        assert_eq!(entity.id, "ghost_model");
        assert_eq!(entity.kind, "gazebo_model");
        assert!(entity.position.is_none());
        assert!(entity.orientation.is_none());
        assert_eq!(entity.frame_id, "world");
        assert!(entity.velocity.is_none());
        assert!(entity.properties.is_empty());
        assert!(entity.timestamp_ns.is_none());
        assert!(entity.last_observed_ns.is_none());
        assert_eq!(entity.observation_confidence, 1.0);
    }
}
