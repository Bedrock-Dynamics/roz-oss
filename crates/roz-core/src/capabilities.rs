use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotCapabilities {
    pub robot_type: String,
    pub joints: Vec<String>,
    pub control_modes: Vec<String>,
    pub workspace_bounds: Option<WorkspaceBounds>,
    pub sensors: Vec<String>,
    pub max_velocity: f64,
    pub cameras: Vec<CameraCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceBounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraCapability {
    pub id: String,
    /// Human-readable label (e.g., "USB Webcam")
    #[serde(default)]
    pub label: String,
    pub resolution: [u32; 2],
    pub fps: u32,
    /// Whether hardware encoding is available for this camera
    #[serde(default)]
    pub hw_encoder: bool,
}
