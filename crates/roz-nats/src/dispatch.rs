//! Typed NATS dispatch messages for task invocation between server and worker.
//!
//! These are the wire-format types exchanged over NATS for task lifecycle management.
//! The server publishes [`TaskInvocation`] to start work, and the worker responds with
//! [`TaskResult`]. Tool approval flows use [`ApprovalRequest`] / [`ApprovalResponse`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Internal NATS subject prefix used to route approval responses back to a task worker.
pub const INTERNAL_APPROVAL_SUBJECT_PREFIX: &str = "roz.internal.tasks.approval";
/// Internal NATS subject prefix used to report task lifecycle transitions back to the server.
pub const INTERNAL_TASK_STATUS_SUBJECT_PREFIX: &str = "roz.internal.tasks.status";

/// Subject carrying approval responses for a specific task.
#[must_use]
pub fn approval_subject(task_id: Uuid) -> String {
    format!("{INTERNAL_APPROVAL_SUBJECT_PREFIX}.{task_id}")
}

/// Subject carrying task lifecycle events for a specific task.
#[must_use]
pub fn task_status_subject(task_id: Uuid) -> String {
    format!("{INTERNAL_TASK_STATUS_SUBJECT_PREFIX}.{task_id}")
}

/// Wire event published by workers as task execution progresses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStatusEvent {
    pub task_id: Uuid,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<Uuid>,
}

/// Extract a W3C traceparent string from the current tracing span's `OTel` context.
///
/// Returns `None` if no valid trace context is active (e.g., in tests or when `OTel` is not configured).
/// Format: `"00-{trace_id}-{span_id}-01"`
pub fn current_traceparent() -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    use tracing::Span;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let span = Span::current();
    let cx = span.context();
    let sc = cx.span().span_context().clone();
    if sc.trace_id() == opentelemetry::trace::TraceId::INVALID {
        return None;
    }
    let flags = if sc.is_sampled() { "01" } else { "00" };
    Some(format!("00-{}-{}-{flags}", sc.trace_id(), sc.span_id()))
}

/// How the agent loop should execute a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Pure LLM reasoning + tools (no spatial context).
    #[serde(rename = "react")]
    React,
    /// Spatial context injected into model messages + safety guards.
    #[serde(rename = "ooda_react", alias = "ooda_re_act")]
    OodaReAct,
}

/// Sent from server to worker via NATS to start a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskInvocation {
    pub task_id: Uuid,
    pub tenant_id: String,
    pub prompt: String,
    pub environment_id: Uuid,
    pub safety_policy_id: Option<Uuid>,
    pub host_id: Uuid,
    pub timeout_secs: u32,
    pub mode: ExecutionMode,
    pub parent_task_id: Option<Uuid>,
    pub restate_url: String,
    /// W3C traceparent for distributed trace propagation (e.g., "00-{trace_id}-{span_id}-01").
    /// Populated by the server, consumed by the worker to link spans across NATS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Ordered phase specs for the agent loop. Empty = single React phase (default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<roz_core::phases::PhaseSpec>,
    /// Optional control-interface contract propagated from the spawning session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_interface_manifest: Option<roz_core::embodiment::binding::ControlInterfaceManifest>,
    /// Optional inherited worker delegation scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_scope: Option<roz_core::tasks::DelegationScope>,
}

/// Token counts for a completed task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// Canonical terminal status reported by a worker once task execution ends.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskTerminalStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    SafetyStop,
}

impl TaskTerminalStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::SafetyStop => "safety_stop",
        }
    }
}

impl std::fmt::Display for TaskTerminalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sent from worker back to Restate when a task completes.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskResult {
    pub task_id: Uuid,
    pub status: TaskTerminalStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub cycles: u32,
    pub token_usage: TokenUsage,
}

/// Worker asks for tool approval (human-in-the-loop).
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub task_id: Uuid,
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub reason: String,
}

/// Response to an approval request.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalResponse {
    pub tool_use_id: String,
    pub approved: bool,
    pub modifier: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_invocation_roundtrip() {
        let invocation = TaskInvocation {
            task_id: Uuid::new_v4(),
            tenant_id: "tenant-abc".to_string(),
            prompt: "Pick up the red block".to_string(),
            environment_id: Uuid::new_v4(),
            safety_policy_id: Some(Uuid::new_v4()),
            host_id: Uuid::new_v4(),
            timeout_secs: 300,
            mode: ExecutionMode::OodaReAct,
            parent_task_id: None,
            restate_url: "http://localhost:8080".to_string(),
            traceparent: None,
            phases: vec![],
            control_interface_manifest: None,
            delegation_scope: None,
        };

        let bytes = serde_json::to_vec(&invocation).expect("serialize");
        let deserialized: TaskInvocation = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(invocation, deserialized);
    }

    #[test]
    fn task_result_roundtrip() {
        let result = TaskResult {
            task_id: Uuid::new_v4(),
            status: TaskTerminalStatus::Succeeded,
            output: Some(serde_json::json!({"picked_up": true})),
            error: None,
            cycles: 5,
            token_usage: TokenUsage {
                input_tokens: 1200,
                output_tokens: 350,
                ..Default::default()
            },
        };

        let bytes = serde_json::to_vec(&result).expect("serialize");
        let deserialized: TaskResult = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn approval_request_roundtrip() {
        let request = ApprovalRequest {
            task_id: Uuid::new_v4(),
            tool_use_id: "toolu_01abc".to_string(),
            tool_name: "move_arm".to_string(),
            tool_input: serde_json::json!({"x": 1.0, "y": 2.0, "z": 0.5}),
            reason: "Moving arm to unknown coordinates".to_string(),
        };

        let bytes = serde_json::to_vec(&request).expect("serialize");
        let deserialized: ApprovalRequest = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(request, deserialized);
    }

    #[test]
    fn execution_mode_serde() {
        let react_json = serde_json::to_value(ExecutionMode::React).expect("serialize react");
        assert_eq!(react_json, serde_json::json!("react"));

        let ooda_json = serde_json::to_value(ExecutionMode::OodaReAct).expect("serialize ooda_react");
        assert_eq!(ooda_json, serde_json::json!("ooda_react"));

        // Roundtrip from string
        let react: ExecutionMode = serde_json::from_value(serde_json::json!("react")).expect("deserialize react");
        assert_eq!(react, ExecutionMode::React);

        let ooda: ExecutionMode =
            serde_json::from_value(serde_json::json!("ooda_react")).expect("deserialize ooda_react");
        assert_eq!(ooda, ExecutionMode::OodaReAct);

        let legacy: ExecutionMode =
            serde_json::from_value(serde_json::json!("ooda_re_act")).expect("deserialize ooda_re_act");
        assert_eq!(legacy, ExecutionMode::OodaReAct);
    }

    #[test]
    fn task_result_failure_roundtrip() {
        let task_id = Uuid::new_v4();
        let result = TaskResult {
            task_id,
            status: TaskTerminalStatus::TimedOut,
            output: None,
            error: Some("timeout".to_string()),
            cycles: 0,
            token_usage: TokenUsage::default(),
        };

        // Verify the wire format shape for failure results.
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(json["status"], "timed_out");
        assert_eq!(json["error"], "timeout");
        assert!(json["output"].is_null());
        assert_eq!(json["cycles"], 0);
        assert_eq!(json["token_usage"]["input_tokens"], 0);
        assert_eq!(json["token_usage"]["output_tokens"], 0);

        // Roundtrip through bytes (the actual NATS wire path).
        let bytes = serde_json::to_vec(&result).expect("serialize to bytes");
        let deserialized: TaskResult = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn task_terminal_status_strings_are_canonical() {
        assert_eq!(TaskTerminalStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(TaskTerminalStatus::Failed.as_str(), "failed");
        assert_eq!(TaskTerminalStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(TaskTerminalStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(TaskTerminalStatus::SafetyStop.as_str(), "safety_stop");
    }

    #[test]
    fn task_status_event_roundtrip() {
        let task_id = Uuid::new_v4();
        let host_id = Uuid::new_v4();
        let event = TaskStatusEvent {
            task_id,
            status: "running".into(),
            detail: Some("worker accepted invocation".into()),
            host_id: Some(host_id),
        };

        let bytes = serde_json::to_vec(&event).expect("serialize");
        let deserialized: TaskStatusEvent = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(deserialized, event);
        assert_eq!(
            task_status_subject(task_id),
            format!("{INTERNAL_TASK_STATUS_SUBJECT_PREFIX}.{task_id}")
        );
    }

    #[test]
    fn approval_response_roundtrip() {
        // With modifier present.
        let with_modifier = ApprovalResponse {
            tool_use_id: "toolu_resp_01".to_string(),
            approved: true,
            modifier: Some(serde_json::json!({"max_speed": 0.5})),
        };

        let json = serde_json::to_value(&with_modifier).expect("serialize with modifier");
        assert_eq!(json["approved"], true);
        assert_eq!(json["modifier"]["max_speed"], 0.5);

        let bytes = serde_json::to_vec(&with_modifier).expect("serialize to bytes");
        let deserialized: ApprovalResponse = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(with_modifier, deserialized);

        // With modifier absent.
        let without_modifier = ApprovalResponse {
            tool_use_id: "toolu_resp_02".to_string(),
            approved: false,
            modifier: None,
        };

        let json = serde_json::to_value(&without_modifier).expect("serialize without modifier");
        assert_eq!(json["approved"], false);
        assert!(json["modifier"].is_null());

        let bytes = serde_json::to_vec(&without_modifier).expect("serialize to bytes");
        let deserialized: ApprovalResponse = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(without_modifier, deserialized);
    }

    #[test]
    fn current_traceparent_returns_none_without_otel() {
        // Without OTel configured, should return None (no valid trace context)
        let tp = current_traceparent();
        assert!(tp.is_none());
    }

    #[test]
    fn task_invocation_phases_serde_roundtrip() {
        use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
        let inv = TaskInvocation {
            task_id: Uuid::nil(),
            tenant_id: "t".into(),
            prompt: "test".into(),
            environment_id: Uuid::nil(),
            safety_policy_id: None,
            host_id: Uuid::nil(),
            timeout_secs: 60,
            mode: ExecutionMode::React,
            parent_task_id: None,
            restate_url: "http://localhost:8080".into(),
            traceparent: None,
            phases: vec![
                PhaseSpec {
                    mode: PhaseMode::React,
                    tools: ToolSetFilter::All,
                    trigger: PhaseTrigger::Immediate,
                },
                PhaseSpec {
                    mode: PhaseMode::OodaReAct,
                    tools: ToolSetFilter::Named(vec!["goto".into()]),
                    trigger: PhaseTrigger::OnToolSignal,
                },
            ],
            control_interface_manifest: None,
            delegation_scope: None,
        };
        let json = serde_json::to_string(&inv).unwrap();
        let back: TaskInvocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phases.len(), 2);
        assert_eq!(back.phases[1].trigger, PhaseTrigger::OnToolSignal);
        // Also verify empty phases omitted from JSON (skip_serializing_if)
        let inv_no_phases = TaskInvocation {
            phases: vec![],
            ..inv.clone()
        };
        let json2 = serde_json::to_string(&inv_no_phases).unwrap();
        assert!(!json2.contains("phases"));
    }

    #[test]
    fn task_invocation_optional_fields() {
        let invocation = TaskInvocation {
            task_id: Uuid::new_v4(),
            tenant_id: "tenant-xyz".to_string(),
            prompt: "Navigate to waypoint".to_string(),
            environment_id: Uuid::new_v4(),
            safety_policy_id: None,
            host_id: Uuid::new_v4(),
            timeout_secs: 60,
            mode: ExecutionMode::React,
            parent_task_id: None,
            restate_url: "http://localhost:9070".to_string(),
            traceparent: None,
            phases: vec![],
            control_interface_manifest: None,
            delegation_scope: None,
        };

        // Verify optional fields serialize as null in the wire format.
        let json = serde_json::to_value(&invocation).expect("serialize");
        assert!(json["safety_policy_id"].is_null());
        assert!(json["parent_task_id"].is_null());
        assert_eq!(json["mode"], "react");

        // Roundtrip through bytes.
        let bytes = serde_json::to_vec(&invocation).expect("serialize to bytes");
        let deserialized: TaskInvocation = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(invocation, deserialized);
    }

    #[test]
    fn approval_subject_formats_with_task_id() {
        let task_id = Uuid::nil();
        assert_eq!(
            approval_subject(task_id),
            format!("{INTERNAL_APPROVAL_SUBJECT_PREFIX}.{task_id}")
        );
    }
}
