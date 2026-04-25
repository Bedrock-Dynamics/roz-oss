//! Phase 26.9 Plan 05 — `/tf` + `/roz/telemetry/pose` emit (`Transform3D`).
//! Plan 03 placed signature stubs; Plan 05 replaces the bodies.
#![cfg(feature = "export-rrd")]

/// Emit a `/tf` (`foxglove.FrameTransform`) message as a Rerun `Transform3D`
/// at `/world/{child_frame_id}` (Plan 05 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 05 will replace the body
/// with `prost::Message::decode` + `rec.log(...)` and may surface decode
/// or rerun-log failures.
pub(super) fn emit_tf(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 05 owns transforms.rs (emit_tf)")
}

/// Emit a `/roz/telemetry/pose` (`foxglove.PoseInFrame`) message as a Rerun
/// `Transform3D` at `/world/robot/pose` (Plan 05 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 05 will replace the body
/// with the real decode + log path.
pub(super) fn emit_pose(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 05 owns transforms.rs (emit_pose)")
}
