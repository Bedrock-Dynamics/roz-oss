//! Point-in-time snapshot of the frame tree for audit and replay.

use serde::{Deserialize, Serialize};

use super::frame_tree::FrameTree;
use crate::session::snapshot::FreshnessState;

/// Immutable point-in-time capture of the frame tree.
///
/// Produced by `EmbodimentRuntime::build_frame_snapshot()`. Attached to
/// evidence bundles and session snapshots so reviewers can reconstruct
/// the kinematic state at any point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameGraphSnapshot {
    /// The full frame tree at capture time.
    pub frame_tree: FrameTree,
    /// Monotonic timestamp (nanoseconds) when this snapshot was taken.
    pub timestamp_ns: u64,
    /// How fresh the underlying data is.
    pub freshness: FreshnessState,
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
            frame_tree: tree,
            timestamp_ns: 1_000_000,
            freshness: FreshnessState::Fresh,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: FrameGraphSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.timestamp_ns, 1_000_000);
        assert!(back.frame_tree.frame_exists("world"));
        assert_eq!(back.freshness, FreshnessState::Fresh);
    }
}
