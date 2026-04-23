//! Phase 26 OBS-02: schema descriptor registry.
//!
//! `mcap::Writer::add_schema` wants raw `FileDescriptorSet` bytes containing
//! ONE message type (the target schema) plus its transitive dependencies.
//! The `FileDescriptorSet`s emitted by `crates/roz-server/build.rs`
//! (`foxglove_descriptor.bin`, `roz_v1_descriptor.bin`) contain MANY messages
//! each. This registry decodes both sets at server boot, extracts per-message
//! `FileDescriptorProto` graphs (owner file + transitive imports), and
//! re-encodes each subset into a single-schema `FileDescriptorSet` buffer
//! that `add_schema` accepts. Decoding happens ONCE at `AppState`
//! construction; the registry hands per-session writers cached `&[u8]`
//! slices via `get`.

use std::collections::{HashMap, HashSet};

use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

use crate::observability::{
    McapArchiveError, SCHEMA_COMPRESSED_IMAGE, SCHEMA_COMPRESSED_VIDEO, SCHEMA_FRAME_TRANSFORM,
    SCHEMA_IMAGE_ANNOTATIONS, SCHEMA_LOG, SCHEMA_POINT_CLOUD, SCHEMA_POSE_IN_FRAME, SCHEMA_RAW_IMAGE,
    SCHEMA_SCENE_UPDATE, SCHEMA_SESSION_EVENT, SCHEMA_TASK_LIFECYCLE, SCHEMA_TOOL_CALL,
};

const FOXGLOVE_DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/foxglove_descriptor.bin"));
const ROZ_V1_DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/roz_v1_descriptor.bin"));

/// Per-schema `FileDescriptorSet`-encoded bytes, keyed by fully-qualified
/// message name (e.g. `foxglove.FrameTransform`).
///
/// Constructed once at server boot via `load`; held on `AppState`. Per-session
/// `WriterActor`s call `get` to obtain the descriptor slice that
/// `mcap::Writer::add_schema` expects.
#[derive(Clone, Debug)]
pub struct SchemaDescriptors {
    inner: HashMap<String, Vec<u8>>,
}

impl SchemaDescriptors {
    /// Decode both descriptor sets and build the per-message cache.
    ///
    /// Called once at server startup; stored on `AppState`. Returns an
    /// error if the embedded descriptor bytes fail to decode as
    /// `FileDescriptorSet`, if any of the twelve target schemas is missing
    /// from the union of loaded files, or if re-encoding the extracted
    /// subset fails.
    ///
    /// # Errors
    /// * `McapArchiveError::ProstDecode` — a descriptor file is not a valid
    ///   `FileDescriptorSet`.
    /// * `McapArchiveError::SchemaNotFound` — one of the twelve target FQNs
    ///   is not declared by any loaded file.
    /// * `McapArchiveError::ProstEncode` — prost fails to serialize the
    ///   extracted subset (should not happen in practice).
    pub fn load() -> Result<Self, McapArchiveError> {
        let foxglove = FileDescriptorSet::decode(FOXGLOVE_DESCRIPTOR)?;
        let roz = FileDescriptorSet::decode(ROZ_V1_DESCRIPTOR)?;

        let mut all_files: Vec<FileDescriptorProto> = Vec::new();
        all_files.extend(foxglove.file);
        all_files.extend(roz.file);

        // Dedup by file name. Both foxglove_descriptor.bin and roz_v1_descriptor.bin
        // transitively carry `google/protobuf/timestamp.proto` (and roz-v1 adds
        // `google/protobuf/struct.proto`). Without dedup, `extract_single_schema_fds`
        // emits a FileDescriptorSet with two copies of the same well-known file,
        // which Foxglove Studio's reflection database rejects as
        // "duplicate name 'Timestamp' in Namespace .google.protobuf". First-seen
        // wins: foxglove entries (extended first above) take precedence.
        let mut seen: HashSet<String> = HashSet::new();
        all_files.retain(|f| f.name.as_deref().map_or(true, |name| seen.insert(name.to_string())));

        let targets = [
            SCHEMA_FRAME_TRANSFORM,
            SCHEMA_POSE_IN_FRAME,
            SCHEMA_LOG,
            SCHEMA_SESSION_EVENT,
            SCHEMA_TASK_LIFECYCLE,
            SCHEMA_TOOL_CALL,
            // Phase 26.5 SC2 additions (R-01 honored: CompressedVideo is the H.264
            // target; CompressedImage is registered alongside but has no producer
            // this phase).
            SCHEMA_COMPRESSED_VIDEO,
            SCHEMA_COMPRESSED_IMAGE,
            SCHEMA_RAW_IMAGE,
            SCHEMA_POINT_CLOUD,
            SCHEMA_SCENE_UPDATE,
            SCHEMA_IMAGE_ANNOTATIONS,
        ];

        let mut inner = HashMap::new();
        for schema_name in targets {
            let bytes = extract_single_schema_fds(&all_files, schema_name)?;
            inner.insert(schema_name.to_string(), bytes);
        }
        Ok(Self { inner })
    }

    /// Get descriptor bytes for a schema name.
    ///
    /// # Errors
    /// Returns `McapArchiveError::SchemaNotFound` if the name was not
    /// registered at `load` time.
    pub fn get(&self, schema_name: &str) -> Result<&[u8], McapArchiveError> {
        self.inner
            .get(schema_name)
            .map(Vec::as_slice)
            .ok_or_else(|| McapArchiveError::SchemaNotFound(schema_name.to_string()))
    }
}

/// Given the union of loaded `FileDescriptorProto` entries, extract the
/// minimum `FileDescriptorSet` that contains the target message type plus
/// its transitive imports, and re-encode as bytes for
/// `mcap::Writer::add_schema`.
fn extract_single_schema_fds(all_files: &[FileDescriptorProto], target_fqn: &str) -> Result<Vec<u8>, McapArchiveError> {
    // target_fqn is "package.Message"; split on the last '.'.
    let (pkg, msg_name) = target_fqn
        .rsplit_once('.')
        .ok_or_else(|| McapArchiveError::SchemaNotFound(target_fqn.to_string()))?;

    // Find the file that declares this message.
    let owner = all_files
        .iter()
        .find(|f| {
            f.package.as_deref() == Some(pkg) && f.message_type.iter().any(|m| m.name.as_deref() == Some(msg_name))
        })
        .ok_or_else(|| McapArchiveError::SchemaNotFound(target_fqn.to_string()))?;

    // Transitive closure over `dependency` links.
    let mut need: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![owner.name.clone().unwrap_or_default()];
    while let Some(f) = queue.pop() {
        if !need.insert(f.clone()) {
            continue;
        }
        if let Some(found) = all_files.iter().find(|x| x.name.as_deref() == Some(f.as_str())) {
            for dep in &found.dependency {
                if !need.contains(dep) {
                    queue.push(dep.clone());
                }
            }
        }
    }

    let files: Vec<FileDescriptorProto> = all_files
        .iter()
        .filter(|f| f.name.as_deref().is_some_and(|n| need.contains(n)))
        .cloned()
        .collect();

    let set = FileDescriptorSet { file: files };
    let mut buf = Vec::new();
    set.encode(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{FileDescriptorSet, SchemaDescriptors};
    use crate::observability::{
        McapArchiveError, SCHEMA_COMPRESSED_IMAGE, SCHEMA_COMPRESSED_VIDEO, SCHEMA_FRAME_TRANSFORM,
        SCHEMA_IMAGE_ANNOTATIONS, SCHEMA_LOG, SCHEMA_POINT_CLOUD, SCHEMA_POSE_IN_FRAME, SCHEMA_RAW_IMAGE,
        SCHEMA_SCENE_UPDATE, SCHEMA_SESSION_EVENT, SCHEMA_TASK_LIFECYCLE, SCHEMA_TOOL_CALL,
    };
    use prost::Message;

    #[test]
    fn loads_all_twelve_target_schemas() {
        let reg = SchemaDescriptors::load().expect("descriptor load");
        for name in [
            SCHEMA_FRAME_TRANSFORM,
            SCHEMA_POSE_IN_FRAME,
            SCHEMA_LOG,
            SCHEMA_SESSION_EVENT,
            SCHEMA_TASK_LIFECYCLE,
            SCHEMA_TOOL_CALL,
            // Phase 26.5 additions (R-01):
            SCHEMA_COMPRESSED_VIDEO,
            SCHEMA_COMPRESSED_IMAGE,
            SCHEMA_RAW_IMAGE,
            SCHEMA_POINT_CLOUD,
            SCHEMA_SCENE_UPDATE,
            SCHEMA_IMAGE_ANNOTATIONS,
        ] {
            let bytes = reg.get(name).unwrap_or_else(|_| panic!("missing {name}"));
            assert!(!bytes.is_empty(), "{name} bytes empty");
            // Round-trip: decoding the produced bytes yields a valid
            // FileDescriptorSet that contains the target message.
            let fds = FileDescriptorSet::decode(bytes).expect("valid FileDescriptorSet");
            let (pkg, msg) = name.rsplit_once('.').expect("fqn has package");
            let has_message = fds.file.iter().any(|f| {
                f.package.as_deref() == Some(pkg) && f.message_type.iter().any(|m| m.name.as_deref() == Some(msg))
            });
            assert!(has_message, "{name} missing from FileDescriptorSet after extract");
        }
    }

    #[test]
    fn schema_not_found_returns_error() {
        let reg = SchemaDescriptors::load().expect("descriptor load");
        let err = reg.get("foxglove.DoesNotExist").expect_err("expected SchemaNotFound");
        assert!(matches!(err, McapArchiveError::SchemaNotFound(ref n) if n == "foxglove.DoesNotExist"));
    }

    #[test]
    fn extracted_foxglove_frame_transform_pulls_in_vector3_and_quaternion() {
        // FrameTransform depends on Vector3 and Quaternion — verify the
        // transitive-import walker pulled them into the extracted set so
        // the schema is self-contained from mcap's perspective.
        let reg = SchemaDescriptors::load().expect("descriptor load");
        let bytes = reg.get(SCHEMA_FRAME_TRANSFORM).expect("frame transform");
        let fds = FileDescriptorSet::decode(bytes).expect("valid FileDescriptorSet");

        let has_vector3 = fds.file.iter().any(|f| {
            f.package.as_deref() == Some("foxglove")
                && f.message_type.iter().any(|m| m.name.as_deref() == Some("Vector3"))
        });
        let has_quaternion = fds.file.iter().any(|f| {
            f.package.as_deref() == Some("foxglove")
                && f.message_type.iter().any(|m| m.name.as_deref() == Some("Quaternion"))
        });
        assert!(has_vector3, "Vector3 missing from FrameTransform descriptor closure");
        assert!(
            has_quaternion,
            "Quaternion missing from FrameTransform descriptor closure"
        );
    }

    #[test]
    fn extracted_fds_has_no_duplicate_filenames() {
        // Regression for Phase 26.1: `SchemaDescriptors::load` merges
        // foxglove_descriptor.bin and roz_v1_descriptor.bin, both of which
        // transitively carry `google/protobuf/timestamp.proto` (and now
        // `google/protobuf/duration.proto` via SceneEntity — Phase 26.5
        // addition). Without filename dedup, each extracted per-schema
        // FileDescriptorSet would contain two copies of the same file, and
        // Foxglove Studio rejects this with
        // "duplicate name 'Timestamp' in Namespace .google.protobuf".
        // Verify every one of the twelve target schemas has unique
        // filenames — the first-seen-wins dedup in SchemaDescriptors::load
        // must keep every extracted per-schema FileDescriptorSet's filename
        // list unique.
        let reg = SchemaDescriptors::load().expect("descriptor load");
        for name in [
            SCHEMA_FRAME_TRANSFORM,
            SCHEMA_POSE_IN_FRAME,
            SCHEMA_LOG,
            SCHEMA_SESSION_EVENT,
            SCHEMA_TASK_LIFECYCLE,
            SCHEMA_TOOL_CALL,
            // Phase 26.5 additions:
            SCHEMA_COMPRESSED_VIDEO,
            SCHEMA_COMPRESSED_IMAGE,
            SCHEMA_RAW_IMAGE,
            SCHEMA_POINT_CLOUD,
            SCHEMA_SCENE_UPDATE,
            SCHEMA_IMAGE_ANNOTATIONS,
        ] {
            let bytes = reg.get(name).unwrap_or_else(|_| panic!("missing {name}"));
            let fds = FileDescriptorSet::decode(bytes).expect("valid FileDescriptorSet");
            let mut seen: HashSet<String> = HashSet::new();
            for f in &fds.file {
                let file_name = f.name.as_deref().expect("file name present");
                assert!(
                    seen.insert(file_name.to_string()),
                    "duplicate file '{file_name}' in FileDescriptorSet for schema {name}"
                );
            }
        }
    }

    #[test]
    fn all_schemas_load_without_error() {
        // D-26 safeguard: if a SCHEMA_* constant is declared in
        // observability/mod.rs AND listed in build.rs's compile_protos array
        // BUT accidentally omitted from the targets list in
        // SchemaDescriptors::load, this test fails immediately at boot.
        // Iterate every constant referenced in the targets array above; each
        // must resolve cleanly.
        let reg = SchemaDescriptors::load().expect("descriptor load");
        for name in [
            SCHEMA_FRAME_TRANSFORM,
            SCHEMA_POSE_IN_FRAME,
            SCHEMA_LOG,
            SCHEMA_SESSION_EVENT,
            SCHEMA_TASK_LIFECYCLE,
            SCHEMA_TOOL_CALL,
            SCHEMA_COMPRESSED_VIDEO,
            SCHEMA_COMPRESSED_IMAGE,
            SCHEMA_RAW_IMAGE,
            SCHEMA_POINT_CLOUD,
            SCHEMA_SCENE_UPDATE,
            SCHEMA_IMAGE_ANNOTATIONS,
        ] {
            assert!(
                reg.get(name).is_ok(),
                "schema {name} did not load — missing from schema_registry targets list?"
            );
        }
    }
}
