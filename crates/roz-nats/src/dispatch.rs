//! Typed NATS dispatch messages for task invocation between server and worker.
//!
//! These are the wire-format types exchanged over NATS for task lifecycle management.
//! The server publishes [`TaskInvocation`] to start work, and the worker responds with
//! [`TaskResult`]. Tool approval flows use [`ApprovalRequest`] / [`ApprovalResponse`].

use std::str::FromStr;

use async_nats::{Client, HeaderMap, HeaderValue};
use roz_core::signing::HEADER_NAME;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Internal NATS subject prefix used to route approval responses back to a task worker.
pub const INTERNAL_APPROVAL_SUBJECT_PREFIX: &str = "roz.internal.tasks.approval";
/// Internal NATS subject prefix used to report task lifecycle transitions back to the server.
pub const INTERNAL_TASK_STATUS_SUBJECT_PREFIX: &str = "roz.internal.tasks.status";
/// Internal NATS subject prefix for embodiment change notifications.
pub const INTERNAL_EMBODIMENT_CHANGED_PREFIX: &str = "roz.internal.embodiment.changed";

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

/// Subject carrying embodiment change events for a specific host.
#[must_use]
pub fn embodiment_changed_subject(host_id: Uuid) -> String {
    format!("{INTERNAL_EMBODIMENT_CHANGED_PREFIX}.{host_id}")
}

/// Wire event published when a host's embodiment data changes.
/// Subscribers receive this to know they should re-read from DB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbodimentChangedEvent {
    pub host_id: Uuid,
    pub tenant_id: Uuid,
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
//
// NOTE: Plan 24-12 added `declared_max_linear_m_per_s: Option<f64>` and
// `declared_max_angular_rad_per_s: Option<f64>`, so `Eq` is no longer
// derivable on this struct. `PartialEq` remains; consumers that previously
// required `Eq` (e.g. hash-set membership) already did not use it in
// practice because f64 equality is domain-specific.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    ///
    /// **Deprecated as of Phase 26.3.** W3C trace context now travels via the
    /// NATS `traceparent`/`tracestate` headers (see `roz_nats::trace`). This body
    /// field is retained for one milestone as a rolling-deploy fallback; the
    /// worker reads the header first and falls back to this field only when
    /// headers are absent. Removal deferred per D-08.
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
    /// Phase 24 FS-01: declared upper-bound linear velocity for the invocation.
    /// The pre-dispatch policy gate uses this to evaluate against `PolicyV1`
    /// limits BEFORE the agent loop starts. `None` on legacy (pre-24-12)
    /// messages; `Some(_)` on v3.0+ invocations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_max_linear_m_per_s: Option<f64>,
    /// Phase 24 FS-01: declared upper-bound angular velocity for the invocation.
    /// See `declared_max_linear_m_per_s` — same semantics, angular axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_max_angular_rad_per_s: Option<f64>,
    /// Phase 26.10 FW-01: authoritative embodiment runtime resolved by the
    /// server at dispatch time. Required for OodaReAct controller promotion;
    /// `controller.rs:553` rejects load without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embodiment_runtime: Option<roz_core::embodiment::EmbodimentRuntime>,
}

impl TaskInvocation {
    /// FW-01: canonical constructor. Use this everywhere a `TaskInvocation` is built.
    /// Defaults `embodiment_runtime: None`; callers that need to attach a runtime
    /// (only the server dispatch path for OodaReAct mode) must set it AFTER
    /// construction via the public field.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        task_id: Uuid,
        tenant_id: String,
        prompt: String,
        environment_id: Uuid,
        safety_policy_id: Option<Uuid>,
        host_id: Uuid,
        timeout_secs: u32,
        mode: ExecutionMode,
        parent_task_id: Option<Uuid>,
        restate_url: String,
        traceparent: Option<String>,
        phases: Vec<roz_core::phases::PhaseSpec>,
        control_interface_manifest: Option<roz_core::embodiment::binding::ControlInterfaceManifest>,
        delegation_scope: Option<roz_core::tasks::DelegationScope>,
        declared_max_linear_m_per_s: Option<f64>,
        declared_max_angular_rad_per_s: Option<f64>,
    ) -> Self {
        Self {
            task_id,
            tenant_id,
            prompt,
            environment_id,
            safety_policy_id,
            host_id,
            timeout_secs,
            mode,
            parent_task_id,
            restate_url,
            traceparent,
            phases,
            control_interface_manifest,
            delegation_scope,
            declared_max_linear_m_per_s,
            declared_max_angular_rad_per_s,
            embodiment_runtime: None,
        }
    }
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

/// Errors surfaced by [`publish_signed`].
#[derive(Debug, thiserror::Error)]
pub enum PublishSignedError {
    /// The signature-layer-provided header value contains characters
    /// disallowed by `async_nats::HeaderValue`. In practice this cannot
    /// happen because [`roz_core::signing::SignatureEnvelope::encode_header`]
    /// emits pure URL-safe base64, but we guard it for defense in depth.
    #[error("signature header contained invalid characters")]
    InvalidHeader,
    /// The underlying NATS client rejected the publish (broker closed,
    /// connection dropped, etc.).
    #[error("nats publish failed: {0}")]
    Nats(String),
}

/// Publish a payload on NATS with a pre-built `roz-sig-v1` header attached.
///
/// The header value must be produced by the signing layer — callers in the
/// server path use [`roz_server::signing_gate::SigningGate::sign_outbound`];
/// callers in the worker path use the symmetric worker-side helper
/// (added in Plan 23-07). This helper is transport-only — it does not
/// touch the signing primitives itself so the `roz-nats` crate keeps its
/// narrow dependency surface.
///
/// # Errors
///
/// - [`PublishSignedError::InvalidHeader`] if the header value contains
///   bytes rejected by `async_nats::HeaderValue`. Structurally impossible
///   with the current envelope encoder (URL-safe base64).
/// - [`PublishSignedError::Nats`] on any transport failure.
pub async fn publish_signed(
    client: &Client,
    subject: String,
    payload: Vec<u8>,
    header_value: &str,
) -> Result<(), PublishSignedError> {
    let mut headers = HeaderMap::new();
    let hv = HeaderValue::from_str(header_value).map_err(|_| PublishSignedError::InvalidHeader)?;
    headers.insert(HEADER_NAME, hv);
    // Phase 26.3 D-05: inject W3C traceparent + tracestate alongside roz-sig-v1 so
    // the ~6 publish call sites (task_dispatch, nats_handlers, grpc/agent) propagate
    // trace context with zero caller churn.
    crate::trace::inject_trace_headers(&mut headers);
    client
        .publish_with_headers(subject, headers, payload.into())
        .await
        .map_err(|e| PublishSignedError::Nats(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_invocation_roundtrip() {
        let invocation = TaskInvocation::new(
            Uuid::new_v4(),
            "tenant-abc".to_string(),
            "Pick up the red block".to_string(),
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
            Uuid::new_v4(),
            300,
            ExecutionMode::OodaReAct,
            None,
            "http://localhost:8080".to_string(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );

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
        let inv = TaskInvocation::new(
            Uuid::nil(),
            "t".into(),
            "test".into(),
            Uuid::nil(),
            None,
            Uuid::nil(),
            60,
            ExecutionMode::React,
            None,
            "http://localhost:8080".into(),
            None,
            vec![
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
            None,
            None,
            None,
            None,
        );
        let json = serde_json::to_string(&inv).unwrap();
        let back: TaskInvocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phases.len(), 2);
        assert_eq!(back.phases[1].trigger, PhaseTrigger::OnToolSignal);
        // Also verify empty phases omitted from JSON (skip_serializing_if)
        let mut inv_no_phases = inv.clone();
        inv_no_phases.phases = vec![];
        let json2 = serde_json::to_string(&inv_no_phases).unwrap();
        assert!(!json2.contains("phases"));
    }

    #[test]
    fn task_invocation_optional_fields() {
        let invocation = TaskInvocation::new(
            Uuid::new_v4(),
            "tenant-xyz".to_string(),
            "Navigate to waypoint".to_string(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::React,
            None,
            "http://localhost:9070".to_string(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );

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

    #[test]
    fn embodiment_changed_event_roundtrip() {
        let host_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let event = EmbodimentChangedEvent { host_id, tenant_id };
        let bytes = serde_json::to_vec(&event).expect("serialize");
        let deserialized: EmbodimentChangedEvent = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(deserialized, event);
        assert_eq!(
            embodiment_changed_subject(host_id),
            format!("{INTERNAL_EMBODIMENT_CHANGED_PREFIX}.{host_id}")
        );
    }

    // -----------------------------------------------------------------------
    // Plan 24-12 Task 1: declared velocity fields on TaskInvocation
    // -----------------------------------------------------------------------

    #[test]
    fn task_invocation_legacy_without_declared_velocities_deserializes_to_none() {
        // Serialize a pre-24-12 TaskInvocation shape — matches the bytes
        // emitted by older servers/tests that existed before the two new
        // declared_* fields were added.
        let legacy = serde_json::json!({
            "task_id": Uuid::nil(),
            "tenant_id": "t",
            "prompt": "p",
            "environment_id": Uuid::nil(),
            "safety_policy_id": null,
            "host_id": Uuid::nil(),
            "timeout_secs": 60,
            "mode": "react",
            "parent_task_id": null,
            "restate_url": "http://localhost:8080",
        });
        let parsed: TaskInvocation = serde_json::from_value(legacy).expect("legacy invocation must parse");
        assert!(parsed.declared_max_linear_m_per_s.is_none());
        assert!(parsed.declared_max_angular_rad_per_s.is_none());
    }

    #[test]
    fn task_invocation_serializes_declared_velocities_when_set() {
        let invocation = TaskInvocation::new(
            Uuid::new_v4(),
            "t".into(),
            "p".into(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::React,
            None,
            "http://localhost:8080".into(),
            None,
            vec![],
            None,
            None,
            Some(1.5),
            Some(0.75),
        );
        let json = serde_json::to_value(&invocation).expect("serialize");
        assert_eq!(json["declared_max_linear_m_per_s"], 1.5);
        assert_eq!(json["declared_max_angular_rad_per_s"], 0.75);
        let bytes = serde_json::to_vec(&invocation).expect("serialize bytes");
        let back: TaskInvocation = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back.declared_max_linear_m_per_s, Some(1.5));
        assert_eq!(back.declared_max_angular_rad_per_s, Some(0.75));
    }

    #[test]
    fn task_invocation_skips_declared_velocities_when_none() {
        let invocation = TaskInvocation::new(
            Uuid::new_v4(),
            "t".into(),
            "p".into(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::React,
            None,
            "http://localhost:8080".into(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );
        let json = serde_json::to_string(&invocation).expect("serialize");
        assert!(!json.contains("declared_max_linear_m_per_s"));
        assert!(!json.contains("declared_max_angular_rad_per_s"));
    }

    // -----------------------------------------------------------------------
    // Phase 26.10 Plan 01 Task 1: FW-01 embodiment_runtime field tests
    // -----------------------------------------------------------------------

    /// FW-01: building a minimal `EmbodimentRuntime` for round-trip tests.
    /// Mirrors the posture of `simple_model()` at
    /// `crates/roz-core/src/embodiment/embodiment_runtime.rs:2599` but kept
    /// minimal — empty vecs are valid for compile().
    fn minimal_runtime() -> roz_core::embodiment::EmbodimentRuntime {
        use roz_core::embodiment::frame_tree::{FrameSource, FrameTree};
        use roz_core::embodiment::EmbodimentModel;
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        let mut model = EmbodimentModel {
            model_id: "fw01-test-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![],
            joints: vec![],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["world".into()],
            channel_bindings: vec![],
        };
        model.stamp_digest();
        roz_core::embodiment::EmbodimentRuntime::compile(model, None, None)
    }

    #[test]
    fn task_invocation_new_defaults_runtime_to_none() {
        let inv = TaskInvocation::new(
            Uuid::new_v4(),
            "t".into(),
            "p".into(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::React,
            None,
            "http://localhost:8080".into(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );
        assert!(inv.embodiment_runtime.is_none());
    }

    #[test]
    fn task_invocation_omits_runtime_when_none() {
        let inv = TaskInvocation::new(
            Uuid::new_v4(),
            "t".into(),
            "p".into(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::OodaReAct,
            None,
            "http://localhost:8080".into(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );
        let value = serde_json::to_value(&inv).expect("serialize");
        assert!(
            value.get("embodiment_runtime").is_none() || value["embodiment_runtime"].is_null(),
            "embodiment_runtime must be skipped when None, got: {value}"
        );
    }

    #[test]
    fn task_invocation_roundtrip_with_runtime() {
        let runtime = minimal_runtime();
        let mut inv = TaskInvocation::new(
            Uuid::new_v4(),
            "t".into(),
            "p".into(),
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            60,
            ExecutionMode::OodaReAct,
            None,
            "http://localhost:8080".into(),
            None,
            vec![],
            None,
            None,
            None,
            None,
        );
        inv.embodiment_runtime = Some(runtime);
        let bytes = serde_json::to_vec(&inv).expect("serialize");
        let back: TaskInvocation = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(inv, back);
        assert!(back.embodiment_runtime.is_some());
        // Confirm the runtime survives byte-for-byte round-trip.
        assert_eq!(
            inv.embodiment_runtime.as_ref().unwrap().combined_digest,
            back.embodiment_runtime.as_ref().unwrap().combined_digest
        );
    }
}
