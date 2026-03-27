//! Vision pipeline configuration for edge-to-cloud image routing.

use serde::{Deserialize, Serialize};

/// How to process camera frames before sending to cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionStrategy {
    /// Run YOLO detection on edge, send JSON detections to cloud.
    EdgeDetection,
    /// Compress keyframes, send to cloud VLM at low rate.
    CompressedKeyframes,
    /// Hybrid: edge detection for real-time + keyframes for cloud reasoning.
    Hybrid,
    /// Local only — no cloud upload (privacy mode).
    LocalOnly,
}

/// Vision pipeline configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionConfig {
    /// Processing strategy.
    #[serde(default = "default_strategy")]
    pub strategy: VisionStrategy,
    /// Keyframe resolution (width, height) for cloud upload.
    #[serde(default = "default_resolution")]
    pub keyframe_resolution: (u32, u32),
    /// Keyframe rate in Hz (default 0.2 = one every 5 seconds).
    #[serde(default = "default_keyframe_rate")]
    pub keyframe_rate_hz: f64,
    /// Maximum edge-to-cloud bandwidth in KB/s.
    #[serde(default = "default_max_bandwidth")]
    pub max_bandwidth_kbps: u32,
}

const fn default_strategy() -> VisionStrategy {
    VisionStrategy::Hybrid
}
const fn default_resolution() -> (u32, u32) {
    (512, 512)
}
const fn default_keyframe_rate() -> f64 {
    0.2
}
const fn default_max_bandwidth() -> u32 {
    50
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            keyframe_resolution: default_resolution(),
            keyframe_rate_hz: default_keyframe_rate(),
            max_bandwidth_kbps: default_max_bandwidth(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_config_default_bandwidth() {
        let config = VisionConfig::default();
        assert_eq!(config.max_bandwidth_kbps, 50);
        assert_eq!(config.keyframe_resolution, (512, 512));
    }

    #[test]
    fn vision_config_serde_roundtrip() {
        let config = VisionConfig {
            strategy: VisionStrategy::LocalOnly,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: VisionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.strategy, VisionStrategy::LocalOnly);
    }
}
