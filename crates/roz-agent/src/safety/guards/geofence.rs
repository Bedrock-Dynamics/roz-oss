use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::SpatialContext;
use roz_core::tools::ToolCall;

use crate::safety::SafetyGuard;

/// A named polygon zone with a buffer distance.
pub struct GeofenceZone {
    pub name: String,
    pub polygon: Vec<[f64; 2]>,
    pub buffer_m: f64,
}

/// 3-D workspace bounds for the geofence guard.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceBounds {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
    pub z_min: f64,
    pub z_max: f64,
}

/// Checks tool call coordinates against inclusion/exclusion geofence zones
/// and 3-D workspace bounds with configurable buffer distance.
///
/// - If workspace bounds are defined, the point (x, y, z) must lie within the
///   bounds shrunk inward by `boundary_buffer_m` on every axis.
/// - If inclusion zones are defined, the point must be inside at least one.
/// - If the point is inside any exclusion zone, the action is blocked.
/// - Non-movement tools (no x/y params) always pass.
pub struct GeofenceGuard {
    inclusion: Vec<GeofenceZone>,
    exclusion: Vec<GeofenceZone>,
    boundary_buffer_m: f64,
    bounds: Option<WorkspaceBounds>,
}

impl GeofenceGuard {
    pub const fn new(inclusion: Vec<GeofenceZone>, exclusion: Vec<GeofenceZone>, boundary_buffer_m: f64) -> Self {
        Self {
            inclusion,
            exclusion,
            boundary_buffer_m,
            bounds: None,
        }
    }

    /// Create a guard with explicit 3-D workspace bounds.
    pub const fn with_bounds(
        inclusion: Vec<GeofenceZone>,
        exclusion: Vec<GeofenceZone>,
        boundary_buffer_m: f64,
        bounds: WorkspaceBounds,
    ) -> Self {
        Self {
            inclusion,
            exclusion,
            boundary_buffer_m,
            bounds: Some(bounds),
        }
    }
}

/// Ray-casting point-in-polygon test.
///
/// Counts the number of times a ray from (x,y) going right crosses polygon edges.
/// Odd crossings = inside; even crossings = outside.
fn point_in_polygon(x: f64, y: f64, polygon: &[[f64; 2]]) -> bool {
    let n = polygon.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (polygon[i][0], polygon[i][1]);
        let (xj, yj) = (polygon[j][0], polygon[j][1]);

        // Check if the ray from (x,y) going right crosses the edge (i, j)
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[async_trait]
impl SafetyGuard for GeofenceGuard {
    fn name(&self) -> &'static str {
        "geofence"
    }

    async fn check(&self, action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
        let Some(x) = action.params.get("x").and_then(serde_json::Value::as_f64) else {
            return SafetyVerdict::Allow;
        };
        let Some(y) = action.params.get("y").and_then(serde_json::Value::as_f64) else {
            return SafetyVerdict::Allow;
        };
        let z = action.params.get("z").and_then(serde_json::Value::as_f64);

        // Check 3-D workspace bounds (shrunk by buffer) if defined
        if let Some(b) = &self.bounds {
            let buf = self.boundary_buffer_m;
            let x_lo = b.x_min + buf;
            let x_hi = b.x_max - buf;
            let y_lo = b.y_min + buf;
            let y_hi = b.y_max - buf;
            let z_lo = b.z_min + buf;
            let z_hi = b.z_max - buf;

            if x < x_lo || x > x_hi || y < y_lo || y > y_hi {
                return SafetyVerdict::Block {
                    reason: format!(
                        "point ({x:.1}, {y:.1}) is outside workspace bounds \
                         (effective x [{x_lo:.1}, {x_hi:.1}], y [{y_lo:.1}, {y_hi:.1}])"
                    ),
                };
            }

            if let Some(zv) = z
                && (zv < z_lo || zv > z_hi)
            {
                return SafetyVerdict::Block {
                    reason: format!("z={zv:.1} is outside workspace z bounds (effective [{z_lo:.1}, {z_hi:.1}])"),
                };
            }
        }

        // Check exclusion zones first (2-D polygon check)
        for zone in &self.exclusion {
            if point_in_polygon(x, y, &zone.polygon) {
                return SafetyVerdict::Block {
                    reason: format!("point ({x:.1}, {y:.1}) is inside exclusion zone '{}'", zone.name),
                };
            }
        }

        // Check inclusion zones (if any are defined, point must be in at least one)
        if !self.inclusion.is_empty() {
            let in_any = self.inclusion.iter().any(|zone| point_in_polygon(x, y, &zone.polygon));
            if !in_any {
                return SafetyVerdict::Block {
                    reason: format!("point ({x:.1}, {y:.1}) is outside all inclusion zones"),
                };
            }
        }

        SafetyVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_state() -> SpatialContext {
        SpatialContext::default()
    }

    /// A 10x10 box centered at origin: (0,0) to (10,10)
    fn box_zone() -> GeofenceZone {
        GeofenceZone {
            name: "workspace".to_string(),
            polygon: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            buffer_m: 0.0,
        }
    }

    #[tokio::test]
    async fn point_inside_inclusion_zone_allows() {
        let guard = GeofenceGuard::new(vec![box_zone()], vec![], 0.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 5.0, "y": 5.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn point_outside_inclusion_zone_blocks() {
        let guard = GeofenceGuard::new(vec![box_zone()], vec![], 0.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 15.0, "y": 5.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("outside"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn point_inside_exclusion_zone_blocks() {
        let exclusion = GeofenceZone {
            name: "obstacle".to_string(),
            polygon: vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]],
            buffer_m: 0.0,
        };
        // No inclusion zones (allow everywhere except exclusions)
        let guard = GeofenceGuard::new(vec![], vec![exclusion], 0.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 5.0, "y": 5.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("exclusion") || reason.contains("obstacle"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn non_movement_tools_always_allow() {
        let guard = GeofenceGuard::new(vec![box_zone()], vec![], 0.0);
        let action = ToolCall {
            id: String::new(),
            tool: "read_sensor".to_string(),
            params: json!({"sensor_id": "temp_1"}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    // Unit test for the point_in_polygon function directly
    #[test]
    fn pip_inside_square() {
        let poly = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(point_in_polygon(5.0, 5.0, &poly));
    }

    #[test]
    fn pip_outside_square() {
        let poly = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(!point_in_polygon(15.0, 5.0, &poly));
    }

    #[test]
    fn pip_inside_triangle() {
        let poly = vec![[0.0, 0.0], [10.0, 0.0], [5.0, 10.0]];
        assert!(point_in_polygon(5.0, 3.0, &poly));
    }

    #[test]
    fn pip_outside_triangle() {
        let poly = vec![[0.0, 0.0], [10.0, 0.0], [5.0, 10.0]];
        assert!(!point_in_polygon(0.0, 10.0, &poly));
    }

    fn workspace_bounds() -> WorkspaceBounds {
        WorkspaceBounds {
            x_min: 0.0,
            x_max: 10.0,
            y_min: 0.0,
            y_max: 10.0,
            z_min: 0.0,
            z_max: 5.0,
        }
    }

    #[tokio::test]
    async fn blocks_outside_z_bounds() {
        let guard = GeofenceGuard::with_bounds(vec![], vec![], 0.0, workspace_bounds());
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 5.0, "y": 5.0, "z": 6.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("z=6.0"), "expected z in reason, got: {reason}");
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn buffer_shrinks_effective_bounds() {
        // Workspace is 0..10 on each axis. Buffer of 1.0 shrinks to 1..9.
        // A point at (9.5, 5.0, 2.0) is inside raw bounds but outside effective bounds.
        let guard = GeofenceGuard::with_bounds(vec![], vec![], 1.0, workspace_bounds());
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 9.5, "y": 5.0, "z": 2.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(
                    reason.contains("outside workspace bounds"),
                    "expected bounds reason, got: {reason}"
                );
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn allows_within_3d_bounds() {
        let guard = GeofenceGuard::with_bounds(vec![], vec![], 0.5, workspace_bounds());
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 5.0, "y": 5.0, "z": 2.5}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }
}
