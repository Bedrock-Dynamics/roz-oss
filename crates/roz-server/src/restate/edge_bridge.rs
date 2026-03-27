use serde::{Deserialize, Serialize};

/// Request to invoke a handler on an edge worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeInvocation {
    pub host_id: String,
    pub service: String,
    pub handler: String,
    pub payload: serde_json::Value,
    pub timeout_ms: u64,
}

/// Result from an edge invocation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EdgeResult {
    Success { response: serde_json::Value },
    Timeout { elapsed_ms: u64 },
    HostOffline { host_id: String },
    Error { message: String },
}

impl EdgeInvocation {
    /// Build the NATS subject for this invocation
    pub fn nats_subject(&self) -> String {
        format!("invoke.{}.{}.{}", self.host_id, self.service, self.handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // EdgeInvocation serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn edge_invocation_serde_roundtrip() {
        let inv = EdgeInvocation {
            host_id: "host-abc".to_string(),
            service: "arm_controller".to_string(),
            handler: "move_to".to_string(),
            payload: json!({"x": 1.0, "y": 2.0}),
            timeout_ms: 5000,
        };
        let json_str = serde_json::to_string(&inv).unwrap();
        let deser: EdgeInvocation = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deser.host_id, "host-abc");
        assert_eq!(deser.service, "arm_controller");
        assert_eq!(deser.handler, "move_to");
        assert_eq!(deser.timeout_ms, 5000);
    }

    // -----------------------------------------------------------------------
    // EdgeResult all 4 variants serialize correctly
    // -----------------------------------------------------------------------

    #[test]
    fn edge_result_success_tag() {
        let result = EdgeResult::Success {
            response: json!({"position": [1.0, 2.0]}),
        };
        let json_str = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "success");
        let deser: EdgeResult = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deser, EdgeResult::Success { .. }));
    }

    #[test]
    fn edge_result_timeout_tag() {
        let result = EdgeResult::Timeout { elapsed_ms: 5001 };
        let json_str = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "timeout");
        assert_eq!(value["elapsed_ms"], 5001);
    }

    #[test]
    fn edge_result_host_offline_tag() {
        let result = EdgeResult::HostOffline {
            host_id: "host-xyz".to_string(),
        };
        let json_str = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "host_offline");
        assert_eq!(value["host_id"], "host-xyz");
    }

    #[test]
    fn edge_result_error_tag() {
        let result = EdgeResult::Error {
            message: "connection refused".to_string(),
        };
        let json_str = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["message"], "connection refused");
    }

    // -----------------------------------------------------------------------
    // nats_subject builds correct format
    // -----------------------------------------------------------------------

    #[test]
    fn nats_subject_format() {
        let inv = EdgeInvocation {
            host_id: "host-1".to_string(),
            service: "sensor".to_string(),
            handler: "read".to_string(),
            payload: json!({}),
            timeout_ms: 1000,
        };
        assert_eq!(inv.nats_subject(), "invoke.host-1.sensor.read");
    }

    #[test]
    fn nats_subject_with_complex_ids() {
        let inv = EdgeInvocation {
            host_id: "robot-arm-42".to_string(),
            service: "gripper_controller".to_string(),
            handler: "open_close".to_string(),
            payload: json!(null),
            timeout_ms: 3000,
        };
        assert_eq!(inv.nats_subject(), "invoke.robot-arm-42.gripper_controller.open_close");
    }
}
