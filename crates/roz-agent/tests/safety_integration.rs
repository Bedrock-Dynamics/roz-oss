use roz_agent::safety::guards::{BatteryGuard, GeofenceGuard, GeofenceZone, VelocityLimiter};
use roz_agent::safety::{SafetyResult, SafetyStack};
use roz_core::spatial::{EntityState, SpatialContext};
use roz_core::tools::ToolCall;
use serde_json::json;
use std::collections::HashMap;

/// 10x10 box from (0,0) to (10,10)
fn workspace_zone() -> GeofenceZone {
    GeofenceZone {
        name: "workspace".to_string(),
        polygon: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
        buffer_m: 0.0,
    }
}

fn context_with_battery(pct: f64) -> SpatialContext {
    let mut properties = HashMap::new();
    properties.insert("battery_pct".to_string(), json!(pct));
    SpatialContext {
        entities: vec![EntityState {
            id: "drone_1".to_string(),
            kind: "drone".to_string(),
            position: Some([5.0, 5.0, 10.0]),
            orientation: None,
            velocity: None,
            properties,
            timestamp_ns: None,
            frame_id: None,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn build_stack() -> SafetyStack {
    SafetyStack::new(vec![
        Box::new(VelocityLimiter::new(5.0)),
        Box::new(GeofenceGuard::new(vec![workspace_zone()], vec![], 0.0)),
        Box::new(BatteryGuard::new(30.0, 15.0)),
    ])
}

#[tokio::test]
async fn inside_zone_safe_velocity_good_battery_approved() {
    let stack = build_stack();
    let action = ToolCall {
        id: String::new(),
        tool: "move".to_string(),
        params: json!({"x": 5.0, "y": 5.0, "velocity_ms": 3.0}),
    };
    let result = stack.evaluate(&action, &context_with_battery(80.0)).await;
    match result {
        SafetyResult::Approved(tc) => {
            assert_eq!(tc.params["velocity_ms"].as_f64().unwrap(), 3.0);
        }
        other => panic!("expected Approved, got {:?}", other),
    }
}

#[tokio::test]
async fn inside_zone_excessive_velocity_clamped() {
    let stack = build_stack();
    let action = ToolCall {
        id: String::new(),
        tool: "move".to_string(),
        params: json!({"x": 5.0, "y": 5.0, "velocity_ms": 15.0}),
    };
    let result = stack.evaluate(&action, &context_with_battery(80.0)).await;
    match result {
        SafetyResult::Approved(tc) => {
            // Velocity should be clamped to 5.0
            assert_eq!(tc.params["velocity_ms"].as_f64().unwrap(), 5.0);
        }
        other => panic!("expected Approved with clamped velocity, got {:?}", other),
    }
}

#[tokio::test]
async fn outside_zone_blocked_by_geofence() {
    let stack = build_stack();
    let action = ToolCall {
        id: String::new(),
        tool: "move".to_string(),
        params: json!({"x": 15.0, "y": 5.0, "velocity_ms": 3.0}),
    };
    let result = stack.evaluate(&action, &context_with_battery(80.0)).await;
    match result {
        SafetyResult::Blocked { guard, reason } => {
            assert_eq!(guard, "geofence");
            assert!(reason.contains("outside"));
        }
        other => panic!("expected Blocked by geofence, got {:?}", other),
    }
}

#[tokio::test]
async fn critical_battery_blocked() {
    let stack = build_stack();
    let action = ToolCall {
        id: String::new(),
        tool: "move".to_string(),
        params: json!({"x": 5.0, "y": 5.0, "velocity_ms": 3.0}),
    };
    let result = stack.evaluate(&action, &context_with_battery(10.0)).await;
    match result {
        SafetyResult::Blocked { guard, reason } => {
            assert_eq!(guard, "battery");
            assert!(reason.contains("critically"));
        }
        other => panic!("expected Blocked by battery, got {:?}", other),
    }
}
