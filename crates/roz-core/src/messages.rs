use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::safety::SafetyLevel;

// ---------------------------------------------------------------------------
// TelemetryMsg
// ---------------------------------------------------------------------------

/// A timestamped telemetry sample from a named stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryMsg {
    pub ts: f64,
    pub stream: String,
    pub data: Value,
}

// ---------------------------------------------------------------------------
// CommandMsg
// ---------------------------------------------------------------------------

/// A command sent to a host, optionally associated with a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMsg {
    pub id: String,
    pub command: String,
    pub params: Value,
    pub task_id: Option<String>,
}

// ---------------------------------------------------------------------------
// EventMsg
// ---------------------------------------------------------------------------

/// A generic event notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMsg {
    pub event: String,
    pub detail: Value,
}

// ---------------------------------------------------------------------------
// StreamingEvent
// ---------------------------------------------------------------------------

/// Events streamed over WebSocket to the IDE client during an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamingEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolRequest {
        call_id: String,
        tool: String,
        params: Value,
    },
    ToolResult {
        call_id: String,
        result: Value,
    },
    SpatialUpdate {
        context: Value,
    },
    SafetyEvent {
        level: SafetyLevel,
        message: String,
    },
    Complete {
        reason: String,
    },
    Error {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // TelemetryMsg serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_msg_serde_roundtrip() {
        let msg = TelemetryMsg {
            ts: 1708617600.123,
            stream: "imu.accel".to_string(),
            data: json!({"x": 0.1, "y": 9.8, "z": 0.0}),
        };
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: TelemetryMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg.ts, deserialized.ts);
        assert_eq!(msg.stream, deserialized.stream);
        assert_eq!(msg.data, deserialized.data);
    }

    // -----------------------------------------------------------------------
    // CommandMsg with and without task_id
    // -----------------------------------------------------------------------

    #[test]
    fn command_msg_without_task_id() {
        let msg = CommandMsg {
            id: "cmd-001".to_string(),
            command: "gripper_open".to_string(),
            params: json!({"force": 10.0}),
            task_id: None,
        };
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: CommandMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg.id, deserialized.id);
        assert_eq!(msg.command, deserialized.command);
        assert_eq!(msg.params, deserialized.params);
        assert!(deserialized.task_id.is_none());
    }

    #[test]
    fn command_msg_with_task_id() {
        let msg = CommandMsg {
            id: "cmd-002".to_string(),
            command: "move_to".to_string(),
            params: json!({"x": 1.0, "y": 2.0}),
            task_id: Some("task-abc".to_string()),
        };
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: CommandMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.task_id, Some("task-abc".to_string()));
    }

    // -----------------------------------------------------------------------
    // EventMsg serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn event_msg_serde_roundtrip() {
        let msg = EventMsg {
            event: "battery_low".to_string(),
            detail: json!({"level": 15, "unit": "percent"}),
        };
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: EventMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg.event, deserialized.event);
        assert_eq!(msg.detail, deserialized.detail);
    }

    // -----------------------------------------------------------------------
    // StreamingEvent all variants serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn streaming_event_text_delta_roundtrip() {
        let event = StreamingEvent::TextDelta {
            text: "Hello, world!".to_string(),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["type"], "text_delta");
        match deserialized {
            StreamingEvent::TextDelta { text } => assert_eq!(text, "Hello, world!"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn streaming_event_thinking_delta_roundtrip() {
        let event = StreamingEvent::ThinkingDelta {
            text: "reasoning...".to_string(),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::ThinkingDelta { text } => assert_eq!(text, "reasoning..."),
            _ => panic!("expected ThinkingDelta"),
        }
    }

    #[test]
    fn streaming_event_tool_request_roundtrip() {
        let event = StreamingEvent::ToolRequest {
            call_id: "call-1".to_string(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0}),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::ToolRequest { call_id, tool, params } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "move_arm");
                assert_eq!(params, json!({"x": 1.0}));
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn streaming_event_tool_result_roundtrip() {
        let event = StreamingEvent::ToolResult {
            call_id: "call-1".to_string(),
            result: json!({"status": "ok"}),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::ToolResult { call_id, result } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(result, json!({"status": "ok"}));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn streaming_event_spatial_update_roundtrip() {
        let event = StreamingEvent::SpatialUpdate {
            context: json!({"frame": "world", "position": [1.0, 2.0, 3.0]}),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::SpatialUpdate { context } => {
                assert_eq!(context["frame"], "world");
            }
            _ => panic!("expected SpatialUpdate"),
        }
    }

    #[test]
    fn streaming_event_safety_event_roundtrip() {
        use crate::safety::SafetyLevel;
        let event = StreamingEvent::SafetyEvent {
            level: SafetyLevel::Warning,
            message: "wind speed approaching limit".to_string(),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::SafetyEvent { level, message } => {
                assert_eq!(level, SafetyLevel::Warning);
                assert_eq!(message, "wind speed approaching limit");
            }
            _ => panic!("expected SafetyEvent"),
        }
    }

    #[test]
    fn streaming_event_complete_roundtrip() {
        let event = StreamingEvent::Complete {
            reason: "task finished".to_string(),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::Complete { reason } => assert_eq!(reason, "task finished"),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn streaming_event_error_roundtrip() {
        let event = StreamingEvent::Error {
            message: "connection lost".to_string(),
        };
        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: StreamingEvent = serde_json::from_str(&serialized).unwrap();
        match deserialized {
            StreamingEvent::Error { message } => assert_eq!(message, "connection lost"),
            _ => panic!("expected Error"),
        }
    }
}
