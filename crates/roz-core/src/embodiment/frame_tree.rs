//! Frame tree and kinematic chain representation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A rigid 3D transform: translation + quaternion rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform3D {
    pub translation: [f64; 3],
    /// Quaternion in [w, x, y, z] order.
    pub rotation: [f64; 4],
    pub timestamp_ns: u64,
}

impl Transform3D {
    /// Identity transform (no translation, no rotation).
    #[must_use]
    pub const fn identity() -> Self {
        Self {
            translation: [0.0, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 0,
        }
    }

    /// Compose two transforms: self * other.
    /// Applies `other` first, then `self`.
    #[must_use]
    #[allow(clippy::suboptimal_flops)]
    pub fn compose(&self, other: &Self) -> Self {
        let q1 = self.rotation;
        let q2 = other.rotation;

        // Quaternion multiplication: q1 * q2
        let qw = q1[0] * q2[0] - q1[1] * q2[1] - q1[2] * q2[2] - q1[3] * q2[3];
        let qx = q1[0] * q2[1] + q1[1] * q2[0] + q1[2] * q2[3] - q1[3] * q2[2];
        let qy = q1[0] * q2[2] - q1[1] * q2[3] + q1[2] * q2[0] + q1[3] * q2[1];
        let qz = q1[0] * q2[3] + q1[1] * q2[2] - q1[2] * q2[1] + q1[3] * q2[0];

        // Rotate other's translation by self's rotation, then add self's translation
        let rotated = Self::rotate_point(q1, other.translation);
        let translation = [
            self.translation[0] + rotated[0],
            self.translation[1] + rotated[1],
            self.translation[2] + rotated[2],
        ];

        Self {
            translation,
            rotation: [qw, qx, qy, qz],
            timestamp_ns: self.timestamp_ns.max(other.timestamp_ns),
        }
    }

    /// Inverse of this transform.
    #[must_use]
    pub fn inverse(&self) -> Self {
        // Conjugate of unit quaternion
        let inv_q = [
            self.rotation[0],
            -self.rotation[1],
            -self.rotation[2],
            -self.rotation[3],
        ];
        let inv_t = Self::rotate_point(inv_q, self.translation);
        Self {
            translation: [-inv_t[0], -inv_t[1], -inv_t[2]],
            rotation: inv_q,
            timestamp_ns: self.timestamp_ns,
        }
    }

    /// Rotate a point by a quaternion.
    ///
    /// Implements `q * (0, p) * q^-1` via the rotation matrix expansion.
    #[allow(clippy::many_single_char_names, clippy::suboptimal_flops)]
    fn rotate_point(q: [f64; 4], p: [f64; 3]) -> [f64; 3] {
        let (w, x, y, z) = (q[0], q[1], q[2], q[3]);
        let (px, py, pz) = (p[0], p[1], p[2]);

        // Rotation matrix coefficients derived from quaternion components.
        let xx = x * x;
        let yy = y * y;
        let zz = z * z;
        let xy = x * y;
        let xz = x * z;
        let yz = y * z;
        let wx = w * x;
        let wy = w * y;
        let wz = w * z;

        [
            (1.0 - 2.0 * (yy + zz)) * px + 2.0 * (xy - wz) * py + 2.0 * (xz + wy) * pz,
            2.0 * (xy + wz) * px + (1.0 - 2.0 * (xx + zz)) * py + 2.0 * (yz - wx) * pz,
            2.0 * (xz - wy) * px + 2.0 * (yz + wx) * py + (1.0 - 2.0 * (xx + yy)) * pz,
        ]
    }
}

/// Where a frame's transform comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameSource {
    /// From URDF/SDF model (never changes).
    Static,
    /// From live sensor data (updated at runtime).
    Dynamic,
    /// Computed from forward kinematics.
    Computed,
}

/// A node in the frame tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameNode {
    pub frame_id: String,
    pub parent_id: Option<String>,
    pub static_transform: Transform3D,
    pub source: FrameSource,
}

/// A tree of coordinate frames with transform lookup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameTree {
    frames: BTreeMap<String, FrameNode>,
    root: Option<String>,
}

impl FrameTree {
    /// Create a new empty frame tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a root frame (no parent).
    pub fn set_root(&mut self, frame_id: &str, source: FrameSource) {
        self.root = Some(frame_id.to_string());
        self.frames.insert(
            frame_id.to_string(),
            FrameNode {
                frame_id: frame_id.to_string(),
                parent_id: None,
                static_transform: Transform3D::identity(),
                source,
            },
        );
    }

    /// Add a child frame with a transform `T_parent_child` — the pose of the
    /// child frame expressed in the parent frame's coordinates.
    ///
    /// Convention: stored transforms are always `T_parent_child`.
    /// `lookup_transform(from, to)` computes `T_from_to` by composing/inverting these edges.
    ///
    /// # Errors
    /// Returns an error if the parent frame does not exist.
    pub fn add_frame(
        &mut self,
        frame_id: &str,
        parent_id: &str,
        transform: Transform3D,
        source: FrameSource,
    ) -> Result<(), FrameTreeError> {
        if !self.frames.contains_key(parent_id) {
            return Err(FrameTreeError::ParentNotFound(parent_id.to_string()));
        }
        self.frames.insert(
            frame_id.to_string(),
            FrameNode {
                frame_id: frame_id.to_string(),
                parent_id: Some(parent_id.to_string()),
                static_transform: transform,
                source,
            },
        );
        Ok(())
    }

    /// Update the transform of an existing Dynamic or Computed frame.
    ///
    /// Static frames (from URDF/model) cannot be mutated at runtime — use
    /// `CalibrationOverlay` to apply corrections to static geometry.
    ///
    /// # Errors
    /// Returns an error if the frame does not exist or is Static.
    pub fn update_transform(&mut self, frame_id: &str, transform: Transform3D) -> Result<(), FrameTreeError> {
        let node = self
            .frames
            .get_mut(frame_id)
            .ok_or_else(|| FrameTreeError::FrameNotFound(frame_id.to_string()))?;
        if node.source == FrameSource::Static {
            return Err(FrameTreeError::StaticFrameMutation(frame_id.to_string()));
        }
        node.static_transform = transform;
        Ok(())
    }

    /// Look up the transform from `from` frame to `to` frame.
    /// Returns `T_from_to` such that `p_to = T_from_to * p_from`.
    ///
    /// # Errors
    /// Returns an error if either frame is not found or they don't share a common ancestor.
    pub fn lookup_transform(&self, from: &str, to: &str) -> Result<Transform3D, FrameTreeError> {
        if from == to {
            return Ok(Transform3D::identity());
        }

        // Get chain from `from` to root
        let from_chain = self.chain_to_root(from)?;
        // Get chain from `to` to root
        let to_chain = self.chain_to_root(to)?;

        // Find common ancestor
        let from_set: std::collections::HashSet<&str> = from_chain.iter().map(String::as_str).collect();
        let common = to_chain
            .iter()
            .find(|f| from_set.contains(f.as_str()))
            .ok_or_else(|| FrameTreeError::NoCommonAncestor(from.to_string(), to.to_string()))?
            .clone();

        // Compose transform from `from` up to common ancestor
        let t_common_from = self.compose_chain_to(from, &common)?;

        // Compose transform from `to` up to common ancestor, then invert
        let t_common_to = self.compose_chain_to(to, &common)?;

        // T_from_to = T_from_common * T_common_to = inv(T_common_from) * T_common_to
        Ok(t_common_from.inverse().compose(&t_common_to))
    }

    /// Get the chain of frame IDs from a frame up to the root.
    fn chain_to_root(&self, frame_id: &str) -> Result<Vec<String>, FrameTreeError> {
        let mut chain = Vec::new();
        let mut current = frame_id.to_string();
        let mut visited = std::collections::HashSet::new();

        loop {
            if !visited.insert(current.clone()) {
                return Err(FrameTreeError::CycleDetected(frame_id.to_string()));
            }
            let node = self
                .frames
                .get(&current)
                .ok_or_else(|| FrameTreeError::FrameNotFound(current.clone()))?;
            chain.push(current.clone());
            match &node.parent_id {
                Some(parent) => current = parent.clone(),
                None => break,
            }
        }
        Ok(chain)
    }

    /// Compose transforms from `start` up to `ancestor` (exclusive).
    fn compose_chain_to(&self, start: &str, ancestor: &str) -> Result<Transform3D, FrameTreeError> {
        let mut result = Transform3D::identity();
        let mut current = start.to_string();

        while current != ancestor {
            let node = self
                .frames
                .get(&current)
                .ok_or_else(|| FrameTreeError::FrameNotFound(current.clone()))?;
            result = node.static_transform.compose(&result);
            current = node
                .parent_id
                .clone()
                .ok_or_else(|| FrameTreeError::NoCommonAncestor(start.to_string(), ancestor.to_string()))?;
        }
        Ok(result)
    }

    /// Check if a frame exists.
    #[must_use]
    pub fn frame_exists(&self, frame_id: &str) -> bool {
        self.frames.contains_key(frame_id)
    }

    /// List all frame IDs.
    #[must_use]
    pub fn all_frame_ids(&self) -> Vec<&str> {
        self.frames.keys().map(String::as_str).collect()
    }

    /// Get a frame node by ID.
    #[must_use]
    pub fn get_frame(&self, frame_id: &str) -> Option<&FrameNode> {
        self.frames.get(frame_id)
    }

    /// Get the root frame ID.
    #[must_use]
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }
}

/// Errors from frame tree operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameTreeError {
    #[error("frame not found: {0}")]
    FrameNotFound(String),
    #[error("parent frame not found: {0}")]
    ParentNotFound(String),
    #[error("no common ancestor between {0} and {1}")]
    NoCommonAncestor(String, String),
    #[error("cycle detected at frame: {0}")]
    CycleDetected(String),
    #[error("cannot mutate static frame: {0} — use CalibrationOverlay for static geometry corrections")]
    StaticFrameMutation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPSILON
    }

    fn approx_eq_arr(a: [f64; 3], b: [f64; 3]) -> bool {
        approx_eq(a[0], b[0]) && approx_eq(a[1], b[1]) && approx_eq(a[2], b[2])
    }

    // -- Transform3D --

    #[test]
    fn identity_compose_is_identity() {
        let id = Transform3D::identity();
        let t = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 100,
        };
        let result = id.compose(&t);
        assert!(approx_eq_arr(result.translation, [1.0, 2.0, 3.0]));
    }

    #[test]
    fn compose_with_identity_is_self() {
        let t = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 100,
        };
        let id = Transform3D::identity();
        let result = t.compose(&id);
        assert!(approx_eq_arr(result.translation, [1.0, 2.0, 3.0]));
    }

    #[test]
    fn inverse_of_identity_is_identity() {
        let id = Transform3D::identity();
        let inv = id.inverse();
        assert!(approx_eq_arr(inv.translation, [0.0, 0.0, 0.0]));
        assert!(approx_eq(inv.rotation[0], 1.0));
    }

    #[test]
    fn compose_with_inverse_is_identity() {
        let t = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [1.0, 0.0, 0.0, 0.0], // identity rotation
            timestamp_ns: 0,
        };
        let result = t.compose(&t.inverse());
        assert!(approx_eq_arr(result.translation, [0.0, 0.0, 0.0]));
    }

    #[test]
    fn pure_translation_compose() {
        let t1 = Transform3D {
            translation: [1.0, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 0,
        };
        let t2 = Transform3D {
            translation: [0.0, 2.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 0,
        };
        let result = t1.compose(&t2);
        assert!(approx_eq_arr(result.translation, [1.0, 2.0, 0.0]));
    }

    #[test]
    fn rotation_90deg_around_z() {
        // 90 degrees around Z: q = [cos(45°), 0, 0, sin(45°)]
        let c = std::f64::consts::FRAC_PI_4.cos();
        let s = std::f64::consts::FRAC_PI_4.sin();
        let t = Transform3D {
            translation: [0.0, 0.0, 0.0],
            rotation: [c, 0.0, 0.0, s],
            timestamp_ns: 0,
        };
        // Rotating [1, 0, 0] by 90° around Z should give [0, 1, 0]
        let point_transform = Transform3D {
            translation: [1.0, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 0,
        };
        let result = t.compose(&point_transform);
        assert!(approx_eq(result.translation[0], 0.0));
        assert!(approx_eq(result.translation[1], 1.0));
        assert!(approx_eq(result.translation[2], 0.0));
    }

    #[test]
    fn inverse_of_rotation() {
        let c = std::f64::consts::FRAC_PI_4.cos();
        let s = std::f64::consts::FRAC_PI_4.sin();
        let t = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [c, 0.0, 0.0, s],
            timestamp_ns: 0,
        };
        let result = t.compose(&t.inverse());
        assert!(approx_eq_arr(result.translation, [0.0, 0.0, 0.0]));
        assert!(approx_eq(result.rotation[0], 1.0));
        assert!(approx_eq(result.rotation[1], 0.0));
        assert!(approx_eq(result.rotation[2], 0.0));
        assert!(approx_eq(result.rotation[3], 0.0));
    }

    #[test]
    fn transform_serde_roundtrip() {
        let t = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [0.707, 0.0, 0.707, 0.0],
            timestamp_ns: 42,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: Transform3D = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    // -- FrameTree --

    fn build_simple_tree() -> FrameTree {
        // world -> base_link -> shoulder -> elbow -> wrist
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame(
            "base_link",
            "world",
            Transform3D {
                translation: [0.0, 0.0, 0.5],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Static,
        )
        .unwrap();
        tree.add_frame(
            "shoulder",
            "base_link",
            Transform3D {
                translation: [0.0, 0.0, 0.3],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Computed,
        )
        .unwrap();
        tree.add_frame(
            "elbow",
            "shoulder",
            Transform3D {
                translation: [0.3, 0.0, 0.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Computed,
        )
        .unwrap();
        tree.add_frame(
            "wrist",
            "elbow",
            Transform3D {
                translation: [0.2, 0.0, 0.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Computed,
        )
        .unwrap();
        tree
    }

    #[test]
    fn frame_tree_root_exists() {
        let tree = build_simple_tree();
        assert_eq!(tree.root(), Some("world"));
        assert!(tree.frame_exists("world"));
    }

    #[test]
    fn frame_tree_all_frames() {
        let tree = build_simple_tree();
        let ids = tree.all_frame_ids();
        assert_eq!(ids.len(), 5);
        assert!(ids.contains(&"world"));
        assert!(ids.contains(&"wrist"));
    }

    #[test]
    fn lookup_same_frame_is_identity() {
        let tree = build_simple_tree();
        let t = tree.lookup_transform("shoulder", "shoulder").unwrap();
        assert!(approx_eq_arr(t.translation, [0.0, 0.0, 0.0]));
    }

    #[test]
    fn lookup_parent_to_child() {
        let tree = build_simple_tree();
        // world to base_link: should be [0, 0, 0.5]
        let t = tree.lookup_transform("world", "base_link").unwrap();
        assert!(approx_eq_arr(t.translation, [0.0, 0.0, 0.5]));
    }

    #[test]
    fn lookup_child_to_parent() {
        let tree = build_simple_tree();
        let t = tree.lookup_transform("base_link", "world").unwrap();
        assert!(approx_eq_arr(t.translation, [0.0, 0.0, -0.5]));
    }

    #[test]
    fn lookup_across_chain() {
        let tree = build_simple_tree();
        // world to wrist: [0,0,0.5] + [0,0,0.3] + [0.3,0,0] + [0.2,0,0]
        // = [0.5, 0, 0.8]
        let t = tree.lookup_transform("world", "wrist").unwrap();
        assert!(approx_eq(t.translation[0], 0.5));
        assert!(approx_eq(t.translation[1], 0.0));
        assert!(approx_eq(t.translation[2], 0.8));
    }

    #[test]
    fn lookup_sibling_frames() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame(
            "left",
            "world",
            Transform3D {
                translation: [0.0, 1.0, 0.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Static,
        )
        .unwrap();
        tree.add_frame(
            "right",
            "world",
            Transform3D {
                translation: [0.0, -1.0, 0.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Static,
        )
        .unwrap();

        let t = tree.lookup_transform("left", "right").unwrap();
        // left is at y=1, right is at y=-1, so left->right is y=-2
        assert!(approx_eq(t.translation[1], -2.0));
    }

    #[test]
    fn lookup_nonexistent_frame_errors() {
        let tree = build_simple_tree();
        let result = tree.lookup_transform("world", "nonexistent");
        assert!(matches!(result, Err(FrameTreeError::FrameNotFound(_))));
    }

    #[test]
    fn add_frame_with_nonexistent_parent_errors() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        let result = tree.add_frame("child", "nonexistent", Transform3D::identity(), FrameSource::Static);
        assert!(matches!(result, Err(FrameTreeError::ParentNotFound(_))));
    }

    #[test]
    fn update_dynamic_transform_works() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame(
            "camera",
            "world",
            Transform3D::identity(),
            FrameSource::Dynamic, // Dynamic frame — updatable
        )
        .unwrap();
        tree.update_transform(
            "camera",
            Transform3D {
                translation: [1.0, 2.0, 3.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 100,
            },
        )
        .unwrap();
        let t = tree.lookup_transform("world", "camera").unwrap();
        assert!(approx_eq(t.translation[0], 1.0));
        assert!(approx_eq(t.translation[1], 2.0));
    }

    #[test]
    fn update_nonexistent_frame_errors() {
        let mut tree = build_simple_tree();
        let result = tree.update_transform("nonexistent", Transform3D::identity());
        assert!(matches!(result, Err(FrameTreeError::FrameNotFound(_))));
    }

    #[test]
    fn cannot_update_static_frame() {
        let mut tree = build_simple_tree();
        // "world" and "base_link" are Static
        let result = tree.update_transform("world", Transform3D::identity());
        assert!(matches!(result, Err(FrameTreeError::StaticFrameMutation(_))));
        let result = tree.update_transform("base_link", Transform3D::identity());
        assert!(matches!(result, Err(FrameTreeError::StaticFrameMutation(_))));
    }

    #[test]
    fn can_update_computed_frame() {
        let mut tree = build_simple_tree();
        // "shoulder" is Computed
        tree.update_transform(
            "shoulder",
            Transform3D {
                translation: [0.0, 0.0, 0.5],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 100,
            },
        )
        .unwrap();
        let t = tree.lookup_transform("base_link", "shoulder").unwrap();
        assert!(approx_eq(t.translation[2], 0.5));
    }

    #[test]
    fn frame_tree_serde_roundtrip() {
        let tree = build_simple_tree();
        let json = serde_json::to_string(&tree).unwrap();
        let back: FrameTree = serde_json::from_str(&json).unwrap();
        // Verify structure survived
        assert_eq!(back.root(), Some("world"));
        assert!(back.frame_exists("wrist"));
        // Verify transforms survived by looking up
        let t = back.lookup_transform("world", "wrist").unwrap();
        assert!(approx_eq(t.translation[0], 0.5));
    }
}
