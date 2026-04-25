//! Phase 26.9 Plan 06 — `/roz/log`, `/roz/session/events`,
//! `/roz/task/lifecycle`, `/roz/tool/calls` emit (`TextLog`).
//! Plan 03 placed signature stubs; Plan 06 replaces the bodies.
#![cfg(feature = "export-rrd")]

/// Emit a `/roz/log` (`foxglove.Log`) message as a Rerun `TextLog`
/// at `/session/log` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_log(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_log)")
}

/// Emit a `/roz/session/events` (`roz.v1.SessionEventEnvelope`) message as a
/// Rerun `TextLog` at `/session/events/{variant}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_session_event(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_session_event)")
}

/// Emit a `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent`) message as a
/// Rerun `TextLog` at `/session/tasks/{task_id}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_task_lifecycle(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_task_lifecycle)")
}

/// Emit a `/roz/tool/calls` (`roz.v1.ToolCallEvent`) message as a Rerun
/// `TextLog` at `/session/tool_calls/{tool_name}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_tool_call(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_tool_call)")
}
