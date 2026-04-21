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
    CHANNEL_LOG, CHANNEL_POSE, CHANNEL_SESSION_EVENTS, CHANNEL_TASK_LIFECYCLE, CHANNEL_TF, CHANNEL_TOOL_CALLS,
    McapArchiveError, SCHEMA_ENCODING_PROTOBUF, SCHEMA_FRAME_TRANSFORM, SCHEMA_LOG, SCHEMA_POSE_IN_FRAME,
    SCHEMA_SESSION_EVENT, SCHEMA_TASK_LIFECYCLE, SCHEMA_TOOL_CALL,
};

/// Channel ID map — 6 entries, keyed by channel topic.
///
/// Produced by [`register_all_channels`] and retained by the per-session
/// `WriterActor` so that each `WriteCommand::Event` can resolve its target
/// `channel_id` without a hash lookup.
#[derive(Debug, Clone)]
pub struct ChannelIds {
    pub tf: u16,
    pub pose: u16,
    pub log: u16,
    pub session_events: u16,
    pub task_lifecycle: u16,
    pub tool_calls: u16,
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

    let empty = BTreeMap::new();
    let tf = writer.add_channel(tf_schema, CHANNEL_TF, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let pose = writer.add_channel(pose_schema, CHANNEL_POSE, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let log = writer.add_channel(log_schema, CHANNEL_LOG, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let session_events = writer.add_channel(session_schema, CHANNEL_SESSION_EVENTS, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let task_lifecycle = writer.add_channel(task_schema, CHANNEL_TASK_LIFECYCLE, SCHEMA_ENCODING_PROTOBUF, &empty)?;
    let tool_calls = writer.add_channel(tool_schema, CHANNEL_TOOL_CALLS, SCHEMA_ENCODING_PROTOBUF, &empty)?;

    Ok(ChannelIds {
        tf,
        pose,
        log,
        session_events,
        task_lifecycle,
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::{ChannelIds, register_all_channels};
    use crate::observability::schema_registry::SchemaDescriptors;
    use mcap::Writer;
    use std::collections::HashSet;
    use std::io::BufWriter;
    use tempfile::NamedTempFile;

    #[test]
    fn registers_all_six_channels_without_error() {
        let descriptors = SchemaDescriptors::load().expect("descriptor load");
        let tmp = NamedTempFile::new().expect("temp file");
        let file = tmp.reopen().expect("reopen");
        let mut writer = Writer::new(BufWriter::new(file)).expect("writer");
        let ids: ChannelIds = register_all_channels(&mut writer, &descriptors).expect("register");
        let _ = writer.finish().expect("finish");

        // All 6 channel IDs must be distinct — `add_channel` allocates sequentially.
        let ids_vec = [
            ids.tf,
            ids.pose,
            ids.log,
            ids.session_events,
            ids.task_lifecycle,
            ids.tool_calls,
        ];
        let unique: HashSet<_> = ids_vec.iter().copied().collect();
        assert_eq!(unique.len(), 6, "expected 6 distinct channel IDs, got {ids_vec:?}");
    }
}
