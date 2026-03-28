use serde::{Deserialize, Serialize};

/// Opaque camera identifier. Wraps the V4L device index or test pattern name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CameraId(pub String);

impl std::fmt::Display for CameraId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl CameraId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Static information about a discovered camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraInfo {
    pub id: CameraId,
    /// Human-readable label (e.g., "USB Webcam", "Pi Camera Module 3")
    pub label: String,
    /// V4L device path (e.g., "/dev/video0") or "test-pattern"
    pub device_path: String,
    /// Supported resolutions as (width, height) pairs
    pub supported_resolutions: Vec<(u32, u32)>,
    /// Maximum supported framerate
    pub max_fps: u32,
    /// Whether this camera supports hardware encoding via V4L2 M2M
    pub hw_encoder_available: bool,
}

/// Which encoder to use for a camera stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderSelection {
    /// Detect hardware encoder, fall back to software
    #[default]
    Auto,
    /// Force hardware encoder (fails if unavailable)
    Hardware,
    /// Force software encoder (openh264)
    Software,
}

/// Adaptive bitrate profile. Defines the quality ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitrateProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl BitrateProfile {
    pub const HIGH: Self = Self {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 2000,
    };
    pub const MEDIUM: Self = Self {
        width: 640,
        height: 480,
        fps: 15,
        bitrate_kbps: 500,
    };
    pub const LOW: Self = Self {
        width: 320,
        height: 240,
        fps: 10,
        bitrate_kbps: 150,
    };

    pub const LADDER: [Self; 3] = [Self::HIGH, Self::MEDIUM, Self::LOW];
}

/// Camera lifecycle events (published to NATS for the server to relay).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CameraEvent {
    /// Camera device detected and ready
    Added { camera: CameraInfo },
    /// Camera device removed (USB unplug, etc.)
    Removed { camera_id: CameraId },
    /// Camera failed mid-stream (device error, encoder crash)
    Failed { camera_id: CameraId, reason: String },
    /// Adaptive bitrate changed quality tier
    QualityChanged {
        camera_id: CameraId,
        profile: BitrateProfile,
    },
}

/// Camera subsystem errors (domain errors, no IO).
#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("camera not found: {0}")]
    NotFound(CameraId),

    #[error("camera device failed: {reason}")]
    DeviceFailed { camera_id: CameraId, reason: String },

    #[error("encoder not available: {0}")]
    EncoderUnavailable(String),

    #[error("encoder reconfigure failed: {0}")]
    EncoderReconfigure(String),

    #[error("max viewers ({max}) reached for camera {camera_id}")]
    MaxViewers { camera_id: CameraId, max: usize },

    #[error("ICE connection failed: {0}")]
    IceConnectionFailed(String),

    #[error("signaling error: {0}")]
    Signaling(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_id_display() {
        let id = CameraId::new("wrist_cam");
        assert_eq!(id.to_string(), "wrist_cam");
    }

    #[test]
    fn camera_info_serde_roundtrip() {
        let info = CameraInfo {
            id: CameraId::new("cam0"),
            label: "USB Webcam".to_string(),
            device_path: "/dev/video0".to_string(),
            supported_resolutions: vec![(640, 480), (1280, 720)],
            max_fps: 30,
            hw_encoder_available: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: CameraInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, CameraId::new("cam0"));
        assert_eq!(parsed.supported_resolutions.len(), 2);
    }

    #[test]
    fn bitrate_profile_ladder_ordering() {
        assert!(BitrateProfile::HIGH.bitrate_kbps > BitrateProfile::MEDIUM.bitrate_kbps);
        assert!(BitrateProfile::MEDIUM.bitrate_kbps > BitrateProfile::LOW.bitrate_kbps);
    }

    #[test]
    fn camera_event_serde_roundtrip() {
        let event = CameraEvent::QualityChanged {
            camera_id: CameraId::new("cam0"),
            profile: BitrateProfile::MEDIUM,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: CameraEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            CameraEvent::QualityChanged { camera_id, profile } => {
                assert_eq!(camera_id.0, "cam0");
                assert_eq!(profile.bitrate_kbps, 500);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encoder_selection_default_is_auto() {
        assert_eq!(EncoderSelection::default(), EncoderSelection::Auto);
    }

    #[test]
    fn camera_error_display() {
        let err = CameraError::NotFound(CameraId::new("missing"));
        assert_eq!(err.to_string(), "camera not found: missing");
    }
}
