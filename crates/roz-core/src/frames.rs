use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CoordinateFrame
// ---------------------------------------------------------------------------

/// Reference frame for spatial data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum CoordinateFrame {
    #[serde(rename = "enu")]
    Enu,
    #[serde(rename = "ned")]
    Ned,
    #[serde(rename = "body")]
    Body,
    #[serde(rename = "custom")]
    Custom(String),
}

// ---------------------------------------------------------------------------
// UnitSpec
// ---------------------------------------------------------------------------

/// Describes a physical quantity and its unit of measure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitSpec {
    pub quantity: String,
    pub unit: String,
}

// ---------------------------------------------------------------------------
// FrameContract
// ---------------------------------------------------------------------------

/// A registered contract that binds a NATS stream to a coordinate frame and
/// unit specification, enabling automatic validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameContract {
    pub id: Uuid,
    pub tenant_id: String,
    pub host_id: String,
    pub stream_name: String,
    pub frame: CoordinateFrame,
    pub units: Vec<UnitSpec>,
    pub created_at: DateTime<Utc>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // CoordinateFrame
    // -----------------------------------------------------------------------

    #[test]
    fn coordinate_frame_enu_serde_roundtrip() {
        let frame = CoordinateFrame::Enu;
        let json = serde_json::to_string(&frame).unwrap();
        let back: CoordinateFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"enu\""));
    }

    #[test]
    fn coordinate_frame_ned_serde_roundtrip() {
        let frame = CoordinateFrame::Ned;
        let json = serde_json::to_string(&frame).unwrap();
        let back: CoordinateFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"ned\""));
    }

    #[test]
    fn coordinate_frame_body_serde_roundtrip() {
        let frame = CoordinateFrame::Body;
        let json = serde_json::to_string(&frame).unwrap();
        let back: CoordinateFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"body\""));
    }

    #[test]
    fn coordinate_frame_custom_serde_roundtrip() {
        let frame = CoordinateFrame::Custom("my_frame".into());
        let json = serde_json::to_string(&frame).unwrap();
        let back: CoordinateFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"custom\""));
        assert!(json.contains("\"my_frame\""));
    }

    // -----------------------------------------------------------------------
    // UnitSpec
    // -----------------------------------------------------------------------

    #[test]
    fn unit_spec_serde_roundtrip() {
        let spec = UnitSpec {
            quantity: "velocity".into(),
            unit: "m/s".into(),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: UnitSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    // -----------------------------------------------------------------------
    // FrameContract
    // -----------------------------------------------------------------------

    #[test]
    fn frame_contract_serde_roundtrip() {
        let contract = FrameContract {
            id: Uuid::new_v4(),
            tenant_id: "tenant-acme".into(),
            host_id: "host-alpha".into(),
            stream_name: "imu.raw".into(),
            frame: CoordinateFrame::Ned,
            units: vec![
                UnitSpec {
                    quantity: "acceleration".into(),
                    unit: "m/s^2".into(),
                },
                UnitSpec {
                    quantity: "angular_velocity".into(),
                    unit: "rad/s".into(),
                },
            ],
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&contract).unwrap();
        let back: FrameContract = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, contract.id);
        assert_eq!(back.tenant_id, contract.tenant_id);
        assert_eq!(back.host_id, contract.host_id);
        assert_eq!(back.stream_name, contract.stream_name);
        assert_eq!(back.frame, contract.frame);
        assert_eq!(back.units, contract.units);
        assert_eq!(back.created_at, contract.created_at);
    }

    #[test]
    fn frame_contract_with_custom_frame_serde_roundtrip() {
        let contract = FrameContract {
            id: Uuid::new_v4(),
            tenant_id: "tenant-acme".into(),
            host_id: "host-beta".into(),
            stream_name: "lidar.pointcloud".into(),
            frame: CoordinateFrame::Custom("sensor_mount".into()),
            units: vec![UnitSpec {
                quantity: "distance".into(),
                unit: "m".into(),
            }],
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&contract).unwrap();
        let back: FrameContract = serde_json::from_str(&json).unwrap();
        assert_eq!(back.frame, CoordinateFrame::Custom("sensor_mount".into()));
    }
}
