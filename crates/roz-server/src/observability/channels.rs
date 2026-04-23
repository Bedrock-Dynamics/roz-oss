//! Phase 26 OBS-01/OBS-02: up-front registration of all 6 MCAP channels.
//!
//! Per RESEARCH anti-pattern, schemas MUST be registered BEFORE the first
//! message on each channel. Doing this lazily would require a `once_cell`
//! per channel and serialize the hot path.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;

use mcap::Writer;

use crate::observability::schema_registry::SchemaDescriptors;
use crate::observability::{
    CHANNEL_ANNOTATIONS, CHANNEL_LOG, CHANNEL_POINTCLOUD, CHANNEL_POSE, CHANNEL_SCENE_UPDATE, CHANNEL_SESSION_EVENTS,
    CHANNEL_TASK_LIFECYCLE, CHANNEL_TF, CHANNEL_TOOL_CALLS, McapArchiveError, SCHEMA_COMPRESSED_IMAGE,
    SCHEMA_COMPRESSED_VIDEO, SCHEMA_ENCODING_PROTOBUF, SCHEMA_FRAME_TRANSFORM, SCHEMA_IMAGE_ANNOTATIONS, SCHEMA_LOG,
    SCHEMA_POINT_CLOUD, SCHEMA_POSE_IN_FRAME, SCHEMA_RAW_IMAGE, SCHEMA_SCENE_UPDATE, SCHEMA_SESSION_EVENT,
    SCHEMA_TASK_LIFECYCLE, SCHEMA_TOOL_CALL,
};

/// Channel ID map — 9 entries, keyed by channel topic.
///
/// Produced by [`register_all_channels`] and retained by the per-session
/// `WriterActor` so that each `WriteCommand::Event` can resolve its target
/// `channel_id` without a hash lookup.
///
/// Phase 26.5 SC3 added three future-producer channels (pointcloud,
/// scene_update, annotations). These channels are registered so every MCAP
/// file declares them at open time; no producer wires them this phase.
/// Phase 29+ sensor fusion emits through them without reopening the schema
/// contract (D-10).
#[derive(Debug, Clone)]
pub struct ChannelIds {
    pub tf: u16,
    pub pose: u16,
    pub log: u16,
    pub session_events: u16,
    pub task_lifecycle: u16,
    pub tool_calls: u16,
    // Phase 26.5 SC3 additions — future Phase 29+ producers.
    //
    // Attribute choice per D-10 (revision 26.5-blocker-01): the module's own
    // test constructs `ChannelIds { .., pointcloud, scene_update, annotations }`
    // via struct-literal init (a use-site), so under
    // `cargo clippy --all-targets -- -D warnings` the `dead_code` lint does
    // NOT fire for the test target. An expectation-style attribute would then
    // be unfulfilled, triggering `unfulfilled_lint_expectations` which
    // `-D warnings` promotes to an error. The allow form below sidesteps this
    // and matches the repo's 30+ existing precedents in production code.
    #[allow(
        dead_code,
        reason = "Phase 29+ producer wiring lands later; channel pre-registered so MCAP schema contract is stable"
    )]
    pub pointcloud: u16,
    #[allow(
        dead_code,
        reason = "Phase 29+ producer wiring lands later; channel pre-registered so MCAP schema contract is stable"
    )]
    pub scene_update: u16,
    #[allow(
        dead_code,
        reason = "Phase 29+ producer wiring lands later; channel pre-registered so MCAP schema contract is stable"
    )]
    pub annotations: u16,
}

/// Register all 6 schemas and all 6 channels on a freshly-opened Writer.
///
/// Call exactly once per file at writer-open time. Returns the [`ChannelIds`]
/// used for every subsequent `write_to_known_channel`.
///
/// # Errors
/// * `McapArchiveError::McapWrite` — the underlying writer rejected the
///   schema or channel record (e.g. duplicate schema, EOF).
/// * `McapArchiveError::SchemaNotFound` — a target schema FQN is absent from
///   the descriptor registry (should not happen after `SchemaDescriptors::load`
///   succeeds).
pub fn register_all_channels(
    writer: &mut Writer<BufWriter<File>>,
    descriptors: &SchemaDescriptors,
) -> Result<ChannelIds, McapArchiveError> {
    // Existing 6 schemas — unchanged.
    let tf_schema = writer.add_schema(
        SCHEMA_FRAME_TRANSFORM,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_FRAME_TRANSFORM)?,
    )?;
    let pose_schema = writer.add_schema(
        SCHEMA_POSE_IN_FRAME,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_POSE_IN_FRAME)?,
    )?;
    let log_schema = writer.add_schema(SCHEMA_LOG, SCHEMA_ENCODING_PROTOBUF, descriptors.get(SCHEMA_LOG)?)?;
    let session_schema = writer.add_schema(
        SCHEMA_SESSION_EVENT,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_SESSION_EVENT)?,
    )?;
    let task_schema = writer.add_schema(
        SCHEMA_TASK_LIFECYCLE,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_TASK_LIFECYCLE)?,
    )?;
    let tool_schema = writer.add_schema(
        SCHEMA_TOOL_CALL,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_TOOL_CALL)?,
    )?;

    // Phase 26.5 SC3: register all 6 multimedia schemas on every writer even
    // though only 3 have channels referencing them this phase. Unused schemas
    // cost ~100 bytes and keep the MCAP file self-describing for Phase 29+
    // producers + Plan 04's dynamic per-camera channel registration (which
    // calls `register_camera_video_schema` separately — mcap 0.24 dedups same
    // (name, encoding, data) tuples to the same u16 id).
    let _compressed_video_schema = writer.add_schema(
        SCHEMA_COMPRESSED_VIDEO,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_COMPRESSED_VIDEO)?,
    )?;
    let _compressed_image_schema = writer.add_schema(
        SCHEMA_COMPRESSED_IMAGE,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_COMPRESSED_IMAGE)?,
    )?;
    let _raw_image_schema = writer.add_schema(
        SCHEMA_RAW_IMAGE,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_RAW_IMAGE)?,
    )?;
    let pointcloud_schema = writer.add_schema(
        SCHEMA_POINT_CLOUD,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_POINT_CLOUD)?,
    )?;
    let scene_update_schema = writer.add_schema(
        SCHEMA_SCENE_UPDATE,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_SCENE_UPDATE)?,
    )?;
    let annotations_schema = writer.add_schema(
        SCHEMA_IMAGE_ANNOTATIONS,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_IMAGE_ANNOTATIONS)?,
    )?;

    let empty = BTreeMap::new();
    // Existing 6 channels — unchanged.
    let tf = writer.add_channel(tf_schema, CHANNEL_TF, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let pose = writer.add_channel(pose_schema, CHANNEL_POSE, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let log = writer.add_channel(log_schema, CHANNEL_LOG, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let session_events =
        writer.add_channel(session_schema, CHANNEL_SESSION_EVENTS, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let task_lifecycle = writer.add_channel(task_schema, CHANNEL_TASK_LIFECYCLE, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let tool_calls = writer.add_channel(tool_schema, CHANNEL_TOOL_CALLS, SCHEMA_ENCODING_PROTOBUF, &empty)?;

    // Phase 26.5 SC3 additions — 3 future-producer channels. Camera channels
    // (`/roz/camera/{camera_id}`) are NOT registered here; they are per-camera
    // dynamic and handled by Plan 04's WriterActor dynamic-register helper on
    // first-sighting from ingest_edge.
    let pointcloud = writer.add_channel(pointcloud_schema, CHANNEL_POINTCLOUD, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let scene_update = writer.add_channel(
        scene_update_schema,
        CHANNEL_SCENE_UPDATE,
        SCHEMA_ENCODING_PROTOBUF,
        &empty,
    )?;
    let annotations = writer.add_channel(
        annotations_schema,
        CHANNEL_ANNOTATIONS,
        SCHEMA_ENCODING_PROTOBUF,
        &empty,
    )?;

    Ok(ChannelIds {
        tf,
        pose,
        log,
        session_events,
        task_lifecycle,
        tool_calls,
        pointcloud,
        scene_update,
        annotations,
    })
}

/// Phase 26.5 SC5 helper — register CompressedVideo and return its `u16` id.
///
/// Called both by [`register_all_channels`] (where the id is discarded) and
/// by `WriterActor`'s per-camera channel registration (Plan 04) where the id
/// is needed to parent `/roz/camera/{camera_id}` channels.
///
/// mcap 0.24 `add_schema` dedups on the (name, encoding, data) tuple —
/// calling this after [`register_all_channels`] returns the same `u16` as
/// the `_compressed_video_schema` binding above (RESEARCH A5 confirms,
/// asserted by `register_camera_video_schema_is_idempotent`).
///
/// # Errors
/// * [`McapArchiveError::McapWrite`] — writer rejected the schema.
/// * [`McapArchiveError::SchemaNotFound`] — descriptors registry missing
///   the `foxglove.CompressedVideo` descriptor (should not happen after a
///   successful [`SchemaDescriptors::load`]).
pub fn register_camera_video_schema(
    writer: &mut Writer<BufWriter<File>>,
    descriptors: &SchemaDescriptors,
) -> Result<u16, McapArchiveError> {
    Ok(writer.add_schema(
        SCHEMA_COMPRESSED_VIDEO,
        SCHEMA_ENCODING_PROTOBUF,
        descriptors.get(SCHEMA_COMPRESSED_VIDEO)?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::{ChannelIds, register_all_channels, register_camera_video_schema};
    use crate::observability::schema_registry::SchemaDescriptors;
    use mcap::Writer;
    use std::collections::HashSet;
    use std::io::BufWriter;
    use tempfile::NamedTempFile;

    #[test]
    fn registers_all_nine_channels_without_error() {
        let descriptors = SchemaDescriptors::load().expect("descriptor load");
        let tmp = NamedTempFile::new().expect("temp file");
        let file = tmp.reopen().expect("reopen");
        let mut writer = Writer::new(BufWriter::new(file)).expect("writer");
        let ids: ChannelIds = register_all_channels(&mut writer, &descriptors).expect("register");
        let _ = writer.finish().expect("finish");

        // All 9 channel IDs must be distinct — `add_channel` allocates sequentially.
        let ids_vec = [
            ids.tf,
            ids.pose,
            ids.log,
            ids.session_events,
            ids.task_lifecycle,
            ids.tool_calls,
            ids.pointcloud,
            ids.scene_update,
            ids.annotations,
        ];
        let unique: HashSet<_> = ids_vec.iter().copied().collect();
        assert_eq!(unique.len(), 9, "expected 9 distinct channel IDs, got {ids_vec:?}");
    }

    #[test]
    fn register_camera_video_schema_is_idempotent() {
        // mcap 0.24 dedups add_schema on (name, encoding, data) tuple. Calling
        // register_camera_video_schema AFTER register_all_channels must return
        // the same u16 that register_all_channels' internal add_schema returned.
        let descriptors = SchemaDescriptors::load().expect("descriptor load");
        let tmp = NamedTempFile::new().expect("temp file");
        let file = tmp.reopen().expect("reopen");
        let mut writer = Writer::new(BufWriter::new(file)).expect("writer");
        let _ids = register_all_channels(&mut writer, &descriptors).expect("register");
        let id1 = register_camera_video_schema(&mut writer, &descriptors).expect("register video schema");
        let id2 = register_camera_video_schema(&mut writer, &descriptors).expect("re-register video schema");
        assert_eq!(
            id1, id2,
            "add_schema should return same u16 on identical re-registration"
        );
        let _ = writer.finish().expect("finish");
    }
}
