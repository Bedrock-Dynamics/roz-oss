use serde::{Deserialize, Serialize};

/// Shape of a workspace zone boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceShape {
    Box { half_extents: [f64; 3] },
    Sphere { radius: f64 },
    Cylinder { radius: f64, half_height: f64 },
}

/// What kind of zone this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneType {
    Allowed,
    Restricted,
    HumanPresence,
}

/// A named workspace zone with shape, frame reference, and margin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceZone {
    pub name: String,
    pub shape: WorkspaceShape,
    pub origin_frame: String,
    pub zone_type: ZoneType,
    pub margin_m: f64,
}

/// A workspace envelope defines the full safe operating boundary.
/// Contains the allowed zone(s) and any restricted sub-zones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceEnvelope {
    pub zones: Vec<WorkspaceZone>,
}

impl WorkspaceShape {
    /// Check if a point (in the shape's local frame) is inside the shape.
    #[must_use]
    pub fn contains_point(&self, point: [f64; 3]) -> bool {
        match self {
            Self::Box { half_extents } => {
                point[0].abs() <= half_extents[0]
                    && point[1].abs() <= half_extents[1]
                    && point[2].abs() <= half_extents[2]
            }
            Self::Sphere { radius } => {
                let dist_sq = point[2].mul_add(point[2], point[0].mul_add(point[0], point[1] * point[1]));
                dist_sq <= radius * radius
            }
            Self::Cylinder { radius, half_height } => {
                let radial_sq = point[0].mul_add(point[0], point[1] * point[1]);
                radial_sq <= radius * radius && point[2].abs() <= *half_height
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_contains_origin() {
        let shape = WorkspaceShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        };
        assert!(shape.contains_point([0.0, 0.0, 0.0]));
    }

    #[test]
    fn box_contains_corner() {
        let shape = WorkspaceShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        };
        assert!(shape.contains_point([1.0, 1.0, 1.0]));
    }

    #[test]
    fn box_rejects_outside() {
        let shape = WorkspaceShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        };
        assert!(!shape.contains_point([1.1, 0.0, 0.0]));
    }

    #[test]
    fn sphere_contains_origin() {
        let shape = WorkspaceShape::Sphere { radius: 0.5 };
        assert!(shape.contains_point([0.0, 0.0, 0.0]));
    }

    #[test]
    fn sphere_contains_surface() {
        let shape = WorkspaceShape::Sphere { radius: 1.0 };
        assert!(shape.contains_point([1.0, 0.0, 0.0]));
    }

    #[test]
    fn sphere_rejects_outside() {
        let shape = WorkspaceShape::Sphere { radius: 1.0 };
        assert!(!shape.contains_point([0.8, 0.8, 0.8])); // sqrt(1.92) > 1.0
    }

    #[test]
    fn cylinder_contains_center() {
        let shape = WorkspaceShape::Cylinder {
            radius: 1.0,
            half_height: 0.5,
        };
        assert!(shape.contains_point([0.0, 0.0, 0.0]));
    }

    #[test]
    fn cylinder_rejects_above() {
        let shape = WorkspaceShape::Cylinder {
            radius: 1.0,
            half_height: 0.5,
        };
        assert!(!shape.contains_point([0.0, 0.0, 0.6]));
    }

    #[test]
    fn cylinder_rejects_radially() {
        let shape = WorkspaceShape::Cylinder {
            radius: 1.0,
            half_height: 0.5,
        };
        assert!(!shape.contains_point([0.8, 0.8, 0.0])); // sqrt(1.28) > 1.0
    }

    #[test]
    fn workspace_zone_serde_roundtrip() {
        let zone = WorkspaceZone {
            name: "safe_area".into(),
            shape: WorkspaceShape::Box {
                half_extents: [1.0, 0.5, 0.8],
            },
            origin_frame: "base_link".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.05,
        };
        let json = serde_json::to_string(&zone).unwrap();
        let back: WorkspaceZone = serde_json::from_str(&json).unwrap();
        assert_eq!(zone, back);
    }

    #[test]
    fn workspace_envelope_serde_roundtrip() {
        let envelope = WorkspaceEnvelope {
            zones: vec![
                WorkspaceZone {
                    name: "allowed".into(),
                    shape: WorkspaceShape::Sphere { radius: 1.5 },
                    origin_frame: "base_link".into(),
                    zone_type: ZoneType::Allowed,
                    margin_m: 0.1,
                },
                WorkspaceZone {
                    name: "human_zone".into(),
                    shape: WorkspaceShape::Box {
                        half_extents: [0.5, 0.5, 1.0],
                    },
                    origin_frame: "world".into(),
                    zone_type: ZoneType::HumanPresence,
                    margin_m: 0.2,
                },
            ],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: WorkspaceEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, back);
    }

    #[test]
    fn shape_variant_tags_in_json() {
        let sphere = WorkspaceShape::Sphere { radius: 1.0 };
        let json = serde_json::to_string(&sphere).unwrap();
        assert!(json.contains("sphere"));
        assert!(json.contains("radius"));

        let bbox = WorkspaceShape::Box {
            half_extents: [1.0, 2.0, 3.0],
        };
        let json = serde_json::to_string(&bbox).unwrap();
        assert!(json.contains("box"));
    }
}
