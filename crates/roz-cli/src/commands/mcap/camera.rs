//! Phase 26.9 Plan 07 — `/roz/camera/{name}` emit (`VideoStream` once +
//! `VideoSample` per frame). Plan 03 placed signature stubs; Plan 07
//! replaces the body.
#![cfg(feature = "export-rrd")]

/// Emit a `/roz/camera/{name}` (`foxglove.CompressedVideo`) frame as a
/// Rerun `VideoSample` at `/world/cameras/{name}`. The `VideoStream`
/// archetype is logged exactly once per camera entity (tracked via
/// `state.seen_camera_videostream_logged`). Plan 07 implements per
/// CONTEXT D-11/D-12.
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 07 will replace the body
/// with the real CompressedVideo decode + Annex-B passthrough log path.
pub(super) fn emit_camera(
    _rec: &rerun::RecordingStream,
    _msg: &mcap::Message<'_>,
    _camera_name: &str,
    _state: &mut super::export::ConversionState,
) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 07 owns camera.rs")
}
