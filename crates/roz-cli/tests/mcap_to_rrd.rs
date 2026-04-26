//! Phase 26.9 D-21 — smoke + bulk integration tests for `roz mcap to-rrd`.
//!
//! Drives the full pipeline via a subprocess invocation of the `roz`
//! binary, exercising the real clap parser, real `export-rrd`-gated code
//! path, and real file-sink `RecordingStream::save()` output.
//!
//! # Gate
//!
//! `#[cfg(feature = "export-rrd")]` because the `mcap to-rrd` binary path
//! is itself feature-gated; without the feature the binary returns the
//! D-17 friendly error rather than producing `.rrd` output.
//!
//! # Test strategy
//!
//! 1. `smoke_to_rrd_single_file` — ignored via `#[ignore]`-with-reason per
//!    D-21. Runs against `phase26-sc5-fixture.mcap` if present; asserts
//!    RRF2 magic + non-empty + exit 0 per RESEARCH §Topic 6 strategy A.
//! 2. `bulk_mode_continue_on_error` — always-on. Synthesizes 2 valid
//!    `foxglove.Log` MCAPs inline (mcap::Writer), corrupts a third, runs
//!    `--bulk` + `--output-dir`, asserts exit nonzero AND the surviving
//!    inputs produce .rrd output (D-05).
//!
//! The `rerun view <path>` check (SC5) is a human verification gate
//! recorded in VERIFICATION.md per D-22 — not automated here.
#![cfg(feature = "export-rrd")]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;

use re_log_encoding::DecoderApp;
use re_log_types::LogMsg;
use re_sorbet::ChunkBatch;

/// RESEARCH §Topic 6 strategy A — magic header check.
const RRF2_MAGIC: &[u8; 4] = b"RRF2";

/// Fixture path referenced by D-21. If absent, the single-file smoke
/// test is `#[ignore]`-gated so CI does not fail.
const FIXTURE_MCAP: &str = "tests/fixtures/phase26-sc5-fixture.mcap";

fn cargo_bin(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    path.push("debug");
    path.push(name);
    path
}

fn roz() -> Command {
    Command::new(cargo_bin("roz"))
}

fn assert_rrf2_magic(path: &Path) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read rrd at {}: {e}", path.display()));
    assert!(
        bytes.len() > 4,
        "rrd at {} must not be empty (got {} bytes)",
        path.display(),
        bytes.len()
    );
    assert_eq!(
        &bytes[..4],
        RRF2_MAGIC,
        "rrd at {} must start with RRF2 magic (got {:x?})",
        path.display(),
        &bytes[..4.min(bytes.len())]
    );
}

#[derive(Debug, Default)]
struct RrdSemanticManifest {
    entities: BTreeSet<String>,
    timelines: BTreeSet<String>,
    components: BTreeSet<String>,
}

fn rrd_entities_timelines_components(path: &Path) -> RrdSemanticManifest {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open rrd at {}: {e}", path.display()));
    let reader = std::io::BufReader::new(file);
    let mut manifest = RrdSemanticManifest::default();

    for decoded in DecoderApp::decode_lazy(reader) {
        let msg = decoded.unwrap_or_else(|e| panic!("decode rrd at {}: {e}", path.display()));
        let LogMsg::ArrowMsg(_, arrow_msg) = msg else {
            continue;
        };
        let chunk = ChunkBatch::try_from(&arrow_msg.batch)
            .unwrap_or_else(|e| panic!("decode rerun chunk at {}: {e}", path.display()));
        manifest.entities.insert(chunk.entity_path().to_string());
        for index in chunk.chunk_schema().columns.index_columns() {
            manifest.timelines.insert(index.timeline_name().to_string());
        }
        for component in chunk.chunk_schema().columns.component_columns() {
            manifest.components.insert(component.component.to_string());
            if let Some(component_type) = component.component_type {
                manifest.components.insert(component_type.full_name().to_owned());
            }
            if let Some(archetype) = component.archetype {
                manifest.components.insert(archetype.full_name().to_owned());
            }
        }
    }

    manifest
}

fn assert_rrd_semantics(path: &Path, entity: &str) {
    let manifest = rrd_entities_timelines_components(path);
    assert!(
        manifest.entities.contains(entity),
        "rrd at {} missing entity {entity}; entities={:?}",
        path.display(),
        manifest.entities
    );
    assert!(
        manifest.timelines.contains("publish_time"),
        "rrd at {} missing publish_time timeline; timelines={:?}",
        path.display(),
        manifest.timelines
    );
    assert!(
        manifest.timelines.contains("log_time"),
        "rrd at {} missing log_time timeline; timelines={:?}",
        path.display(),
        manifest.timelines
    );
    assert!(
        !manifest.components.is_empty(),
        "rrd at {} must contain at least one semantic component",
        path.display()
    );
}

// ------------------------------------------------------------------
// Single-file smoke test (D-21 / SC6) — ignored when fixture is absent.
// ------------------------------------------------------------------

#[test]
#[ignore = "Phase 26.9 D-21: requires phase26-sc5-fixture.mcap; run with --ignored after placing fixture at tests/fixtures/"]
fn smoke_to_rrd_single_file() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_MCAP);
    assert!(
        fixture.exists(),
        "fixture not found at {}; D-21 test requires phase26-sc5-fixture.mcap",
        fixture.display()
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let rrd_path = tmp.path().join("fixture.rrd");

    let out = roz()
        .args([
            "mcap",
            "to-rrd",
            fixture.to_str().expect("fixture path utf8"),
            "--output",
            rrd_path.to_str().expect("rrd path utf8"),
        ])
        .output()
        .expect("spawn roz mcap to-rrd");

    assert!(
        out.status.success(),
        "to-rrd failed (status={:?}): stdout={} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_rrf2_magic(&rrd_path);
}

// ------------------------------------------------------------------
// Bulk-mode continue-on-error test (D-05) — always-on.
// ------------------------------------------------------------------

/// Inline varint encoder. Avoids `prost::encoding::encode_varint`, which
/// is `#[doc(hidden)]` upstream. The encoding is the standard protobuf
/// base-128 varint: emit 7-bit groups LSB-first; high bit set on every
/// byte except the last.
fn push_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(((value as u8) & 0x7F) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Encode a minimal `foxglove.Log` proto payload by hand. Field tags
/// (level=2, message=3) match `proto/foxglove/Log.proto` verbatim. Only
/// the two fields `emit_log` actually reads (`level`, `message`) are
/// populated; defaults for the rest are accepted by `prost::Message::decode`.
fn encode_foxglove_log(level: i32, message: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    // field 2 (level), wire type 0 (varint). Tag byte = (field_number << 3) | wire_type.
    buf.push(2 << 3); // wire_type 0 contributes nothing
    push_varint(&mut buf, level as u64);
    // field 3 (message), wire type 2 (length-delimited string)
    buf.push((3 << 3) | 2);
    let body = message.as_bytes();
    push_varint(&mut buf, body.len() as u64);
    buf.extend_from_slice(body);
    buf
}

fn push_len_delimited(buf: &mut Vec<u8>, field_number: u8, body: &[u8]) {
    buf.push((field_number << 3) | 2);
    push_varint(buf, body.len() as u64);
    buf.extend_from_slice(body);
}

fn push_fixed64(buf: &mut Vec<u8>, field_number: u8, value: f64) {
    buf.push((field_number << 3) | 1);
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_timestamp(seconds: i64, nanos: i32) -> Vec<u8> {
    let mut buf = Vec::new();
    // field 1 (seconds), wire type 0.
    buf.push(1 << 3);
    push_varint(&mut buf, seconds as u64);
    // field 2 (nanos), wire type 0.
    buf.push(2 << 3);
    push_varint(&mut buf, nanos as u64);
    buf
}

fn encode_vec3(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut buf = Vec::new();
    push_fixed64(&mut buf, 1, x);
    push_fixed64(&mut buf, 2, y);
    push_fixed64(&mut buf, 3, z);
    buf
}

fn encode_quat(x: f64, y: f64, z: f64, w: f64) -> Vec<u8> {
    let mut buf = Vec::new();
    push_fixed64(&mut buf, 1, x);
    push_fixed64(&mut buf, 2, y);
    push_fixed64(&mut buf, 3, z);
    push_fixed64(&mut buf, 4, w);
    buf
}

fn encode_foxglove_frame_transform(parent: &str, child: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    push_len_delimited(&mut buf, 1, &encode_timestamp(1, 0));
    push_len_delimited(&mut buf, 2, parent.as_bytes());
    push_len_delimited(&mut buf, 3, child.as_bytes());
    push_len_delimited(&mut buf, 4, &encode_vec3(1.0, 2.0, 3.0));
    push_len_delimited(&mut buf, 5, &encode_quat(0.0, 0.0, 0.0, 1.0));
    buf
}

fn encode_foxglove_pose_in_frame(frame_id: &str) -> Vec<u8> {
    let mut pose = Vec::new();
    push_len_delimited(&mut pose, 1, &encode_vec3(4.0, 5.0, 6.0));
    push_len_delimited(&mut pose, 2, &encode_quat(0.0, 0.0, 0.0, 1.0));

    let mut buf = Vec::new();
    push_len_delimited(&mut buf, 1, &encode_timestamp(1, 500_000_000));
    push_len_delimited(&mut buf, 2, frame_id.as_bytes());
    push_len_delimited(&mut buf, 3, &pose);
    buf
}

fn encode_foxglove_compressed_video(frame_id: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    push_len_delimited(&mut buf, 1, &encode_timestamp(2, 0));
    push_len_delimited(&mut buf, 2, frame_id.as_bytes());
    push_len_delimited(&mut buf, 3, &[0x00, 0x00, 0x00, 0x01, 0x65, b'r', b'o', b'z']);
    push_len_delimited(&mut buf, 4, b"h264");
    buf
}

/// Write a minimal MCAP file containing a single `/roz/log` message
/// carrying a `foxglove.Log` payload. Uses the workspace `mcap = 0.24`
/// `Writer::new(BufWriter)` constructor (the same shape used by
/// `crates/roz-server/src/observability/mcap_archive.rs`).
fn write_synthetic_mcap(path: &Path, body: &str) {
    let file = std::fs::File::create(path).expect("create mcap");
    let buf = BufWriter::new(file);
    let mut writer = mcap::Writer::new(buf).expect("mcap writer");

    // Schema: empty descriptor bytes — the export classifier inspects
    // schema.name only (D-14), not schema.data.
    let schema_id = writer.add_schema("foxglove.Log", "protobuf", &[]).expect("add schema");
    let channel_id = writer
        .add_channel(schema_id, "/roz/log", "protobuf", &BTreeMap::new())
        .expect("add channel");

    let payload = encode_foxglove_log(2, body); // level=INFO

    writer
        .write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: 0,
                log_time: 1_000_000_000, // 1.0 s since epoch (ns)
                publish_time: 1_000_000_000,
            },
            payload.as_slice(),
        )
        .expect("write message");

    writer.finish().expect("finalize mcap");
}

fn write_semantic_mcap(path: &Path) {
    let file = std::fs::File::create(path).expect("create semantic mcap");
    let buf = BufWriter::new(file);
    let mut writer = mcap::Writer::new(buf).expect("mcap writer");

    let log_schema = writer
        .add_schema("foxglove.Log", "protobuf", &[])
        .expect("add log schema");
    let tf_schema = writer
        .add_schema("foxglove.FrameTransform", "protobuf", &[])
        .expect("add tf schema");
    let pose_schema = writer
        .add_schema("foxglove.PoseInFrame", "protobuf", &[])
        .expect("add pose schema");
    let video_schema = writer
        .add_schema("foxglove.CompressedVideo", "protobuf", &[])
        .expect("add video schema");

    let log = writer
        .add_channel(log_schema, "/roz/log", "protobuf", &BTreeMap::new())
        .expect("add log channel");
    let tf = writer
        .add_channel(tf_schema, "/tf", "protobuf", &BTreeMap::new())
        .expect("add tf channel");
    let pose = writer
        .add_channel(pose_schema, "/roz/telemetry/pose", "protobuf", &BTreeMap::new())
        .expect("add pose channel");
    let camera = writer
        .add_channel(video_schema, "/roz/camera/front", "protobuf", &BTreeMap::new())
        .expect("add camera channel");

    let records = [
        (log, 1_000_000_000, encode_foxglove_log(2, "semantic log")),
        (
            tf,
            1_100_000_000,
            encode_foxglove_frame_transform("world", "robot/base_link"),
        ),
        (pose, 1_200_000_000, encode_foxglove_pose_in_frame("world")),
        (camera, 1_300_000_000, encode_foxglove_compressed_video("front")),
    ];

    for (sequence, (channel_id, log_time, payload)) in records.into_iter().enumerate() {
        writer
            .write_to_known_channel(
                &mcap::records::MessageHeader {
                    channel_id,
                    sequence: u32::try_from(sequence).expect("sequence fits u32"),
                    log_time,
                    publish_time: log_time + 10_000_000,
                },
                payload.as_slice(),
            )
            .expect("write semantic message");
    }

    writer.finish().expect("finalize semantic mcap");
}

#[test]
fn semantic_rrd_contains_entities_timelines_and_components() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mcap_path = tmp.path().join("semantic.mcap");
    let rrd_path = tmp.path().join("semantic.rrd");
    write_semantic_mcap(&mcap_path);

    let out = roz()
        .args([
            "mcap",
            "to-rrd",
            mcap_path.to_str().expect("mcap path utf8"),
            "--output",
            rrd_path.to_str().expect("rrd path utf8"),
        ])
        .output()
        .expect("spawn roz mcap to-rrd");

    assert!(
        out.status.success(),
        "to-rrd failed (status={:?}): stdout={} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_rrf2_magic(&rrd_path);
    let manifest = rrd_entities_timelines_components(&rrd_path);
    for entity in [
        "/session/log",
        "/world/robot/base_link",
        "/world/robot/pose",
        "/world/cameras/front",
    ] {
        assert!(
            manifest.entities.contains(entity),
            "rrd missing entity {entity}; entities={:?}",
            manifest.entities
        );
    }
    assert!(
        manifest.timelines.contains("publish_time"),
        "rrd missing publish_time timeline; timelines={:?}",
        manifest.timelines
    );
    assert!(
        manifest.timelines.contains("log_time"),
        "rrd missing log_time timeline; timelines={:?}",
        manifest.timelines
    );
    assert!(
        manifest.components.iter().any(|c| c.contains("Text")),
        "rrd missing text/log component; components={:?}",
        manifest.components
    );
    assert!(
        manifest
            .components
            .iter()
            .any(|c| c.contains("Transform") || c.contains("Translation") || c.contains("Rotation")),
        "rrd missing transform component; components={:?}",
        manifest.components
    );
    assert!(
        manifest.components.iter().any(|c| c.contains("Video")),
        "rrd missing video component; components={:?}",
        manifest.components
    );
}

#[test]
fn bulk_mode_continue_on_error() {
    let inputs_dir = tempfile::tempdir().expect("inputs tempdir");
    let outputs_dir = tempfile::tempdir().expect("outputs tempdir");

    let good_a = inputs_dir.path().join("good_a.mcap");
    let good_b = inputs_dir.path().join("good_b.mcap");
    let bad = inputs_dir.path().join("bad.mcap");

    write_synthetic_mcap(&good_a, "hello from a");
    write_synthetic_mcap(&good_b, "hello from b");

    // `bad.mcap` is not a real MCAP — it's garbage that MessageStream::new
    // will reject. This exercises D-05 continue-on-error.
    std::fs::write(&bad, b"this is not an mcap").expect("write garbage");

    let pattern = format!("{}/*.mcap", inputs_dir.path().display());
    let out = roz()
        .args([
            "mcap",
            "to-rrd",
            "--bulk",
            &pattern,
            "--output-dir",
            outputs_dir.path().to_str().expect("output-dir utf8"),
        ])
        .output()
        .expect("spawn roz mcap to-rrd --bulk");

    // D-05: exit nonzero because at least one file failed.
    assert!(
        !out.status.success(),
        "bulk mode must exit nonzero when any file fails; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Stderr summary line per D-05 (`{ok}/{total} succeeded, {fail} failed`).
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("/3 succeeded") || stderr.contains("/3 "),
        "stderr should contain '/3 succeeded' summary; got: {stderr}"
    );
    assert!(
        stderr.contains("[ERR]") || stderr.contains("failed"),
        "stderr should indicate per-file error; got: {stderr}"
    );

    // Surviving .rrd outputs:
    let out_a = outputs_dir.path().join("good_a.rrd");
    let out_b = outputs_dir.path().join("good_b.rrd");

    assert!(out_a.exists(), "good_a.rrd must exist after bulk run");
    assert!(out_b.exists(), "good_b.rrd must exist after bulk run");

    // Both good outputs must carry RRF2 magic.
    assert_rrf2_magic(&out_a);
    assert_rrf2_magic(&out_b);
    assert_rrd_semantics(&out_a, "/session/log");
    assert_rrd_semantics(&out_b, "/session/log");

    // Note: `bad.rrd` may also exist with just a Rerun header, because
    // `export_one` opens the writer (writing the RRF2 magic) BEFORE
    // iterating the MCAP message stream, so the writer file is created on
    // disk before the per-message MCAP decode error surfaces. The D-05
    // continue-on-error contract (surviving files succeed, failing files
    // exit nonzero) is verified above; the on-disk shape of failed-input
    // outputs is implementation-defined.
}
