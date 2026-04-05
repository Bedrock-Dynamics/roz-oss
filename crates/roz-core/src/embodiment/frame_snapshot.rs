//! Point-in-time snapshot of the frame tree for audit and replay.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::frame_tree::{FrameSource, FrameTree, Transform3D};
use crate::clock::ClockDomain;
use crate::session::snapshot::FreshnessState;

const fn default_frame_snapshot_clock_domain() -> ClockDomain {
    ClockDomain::Monotonic
}

/// Timestamped transform captured for a specific frame at snapshot time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimestampedTransform {
    pub frame_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub transform: Transform3D,
    pub freshness: FreshnessState,
    pub source: FrameSource,
}

/// External world anchor fused into the frame graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldAnchor {
    pub anchor_id: String,
    pub frame_id: String,
    pub transform: Transform3D,
    pub source: String,
    pub confidence: f64,
}

/// Typed runtime input used to construct a frame-graph snapshot.
///
/// This keeps freshness, provenance, and world-anchor data explicit instead
/// of requiring `EmbodimentRuntime` to infer them from raw transforms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSnapshotInput {
    #[serde(default = "default_frame_snapshot_clock_domain")]
    pub clock_domain: ClockDomain,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub joint_positions: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_transforms: Vec<TimestampedTransform>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub world_anchors: Vec<WorldAnchor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_issues: Vec<String>,
}

impl Default for FrameSnapshotInput {
    fn default() -> Self {
        Self {
            clock_domain: default_frame_snapshot_clock_domain(),
            joint_positions: BTreeMap::new(),
            dynamic_transforms: Vec::new(),
            world_anchors: Vec::new(),
            validation_issues: Vec::new(),
        }
    }
}

/// Immutable point-in-time capture of the frame tree.
///
/// Produced by `EmbodimentRuntime::build_frame_snapshot()`. Attached to
/// evidence bundles and session snapshots so reviewers can reconstruct
/// the kinematic state at any point in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameGraphSnapshot {
    /// Monotonic snapshot sequence. Defaults to 0 for older call sites.
    #[serde(default)]
    pub snapshot_id: u64,
    /// Monotonic timestamp (nanoseconds) when this snapshot was taken.
    pub timestamp_ns: u64,
    /// Which clock domain both `timestamp_ns` and freshness judgments use.
    #[serde(default = "default_frame_snapshot_clock_domain")]
    pub clock_domain: ClockDomain,
    /// The full frame tree at capture time.
    pub frame_tree: FrameTree,
    /// How fresh the underlying data is.
    pub freshness: FreshnessState,
    /// Digest of the base embodiment model bound to this snapshot.
    #[serde(default)]
    pub model_digest: String,
    /// Digest of the active calibration overlay, if any.
    #[serde(default)]
    pub calibration_digest: String,
    /// Active calibration identity, if a valid calibration overlay was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_calibration_id: Option<String>,
    /// Dynamic transforms captured at this point in time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_transforms: Vec<TimestampedTransform>,
    /// Frames projected into controller-facing watched poses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_frames: Vec<String>,
    /// Per-frame freshness state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub frame_freshness: BTreeMap<String, FreshnessState>,
    /// Unique transform sources present in the snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<FrameSource>,
    /// External world anchors bound into this snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub world_anchors: Vec<WorldAnchor>,
    /// Validation or degradation notes captured while assembling the snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_issues: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::frame_tree::FrameSource;

    #[test]
    fn frame_graph_snapshot_serde_roundtrip() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);

        let snap = FrameGraphSnapshot {
            snapshot_id: 7,
            timestamp_ns: 1_000_000,
            clock_domain: ClockDomain::Monotonic,
            frame_tree: tree,
            freshness: FreshnessState::Fresh,
            model_digest: "model-sha".into(),
            calibration_digest: "cal-sha".into(),
            active_calibration_id: Some("cal-1".into()),
            dynamic_transforms: vec![TimestampedTransform {
                frame_id: "camera".into(),
                parent_id: Some("world".into()),
                transform: Transform3D {
                    translation: [0.1, 0.2, 0.3],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 1_000_000,
                },
                freshness: FreshnessState::Fresh,
                source: FrameSource::Dynamic,
            }],
            watched_frames: vec!["world".into(), "camera".into()],
            frame_freshness: BTreeMap::from([
                ("world".into(), FreshnessState::Fresh),
                ("camera".into(), FreshnessState::Fresh),
            ]),
            sources: vec![FrameSource::Static, FrameSource::Dynamic],
            world_anchors: vec![WorldAnchor {
                anchor_id: "anchor-1".into(),
                frame_id: "world".into(),
                transform: Transform3D::identity(),
                source: "slam".into(),
                confidence: 0.9,
            }],
            validation_issues: vec!["camera transform supplied from replay cache".into()],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: FrameGraphSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.snapshot_id, 7);
        assert_eq!(back.timestamp_ns, 1_000_000);
        assert_eq!(back.clock_domain, ClockDomain::Monotonic);
        assert!(back.frame_tree.frame_exists("world"));
        assert_eq!(back.freshness, FreshnessState::Fresh);
        assert_eq!(back.model_digest, "model-sha");
        assert_eq!(back.calibration_digest, "cal-sha");
        assert_eq!(back.active_calibration_id.as_deref(), Some("cal-1"));
        assert_eq!(back.dynamic_transforms.len(), 1);
        assert_eq!(back.watched_frames.len(), 2);
        assert_eq!(back.frame_freshness.get("camera"), Some(&FreshnessState::Fresh));
        assert_eq!(back.sources.len(), 2);
        assert_eq!(back.world_anchors.len(), 1);
        assert_eq!(back.validation_issues.len(), 1);
    }

    #[test]
    fn frame_snapshot_input_serde_roundtrip() {
        let input = FrameSnapshotInput {
            clock_domain: ClockDomain::Monotonic,
            joint_positions: BTreeMap::from([("shoulder_pitch".into(), 0.25)]),
            dynamic_transforms: vec![TimestampedTransform {
                frame_id: "camera".into(),
                parent_id: Some("world".into()),
                transform: Transform3D {
                    translation: [0.1, 0.0, 0.2],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 42,
                },
                freshness: FreshnessState::Fresh,
                source: FrameSource::Dynamic,
            }],
            world_anchors: vec![WorldAnchor {
                anchor_id: "anchor-1".into(),
                frame_id: "world".into(),
                transform: Transform3D::identity(),
                source: "slam".into(),
                confidence: 0.8,
            }],
            validation_issues: vec!["replay supplied external anchor".into()],
        };

        let json = serde_json::to_string(&input).unwrap();
        let back: FrameSnapshotInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, input);
    }
}
