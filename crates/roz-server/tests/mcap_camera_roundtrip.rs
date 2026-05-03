//! Phase 26.5 SC7: integration test proving synthetic H.264 camera frames
//! round-trip through the `WriterActor` into a per-session MCAP file and
//! re-decode cleanly as `foxglove.CompressedVideo` (R-01 honored — the
//! ROADMAP SC7 wording "`CompressedImage`" is stale; H.264 targets
//! `CompressedVideo` per Foxglove Studio's H.264 renderer contract).
//!
//! Also includes a cross-crate wire-compat test (revision 26.5-warning-02)
//! that verifies the worker's hand-vendored prost struct at
//! `roz_worker::camera::mcap_relay::CompressedVideo` produces bytes
//! decodable by the server-side `foxglove::CompressedVideo` from Plan 01's
//! tonic codegen. Closes the silent-field-tag-drift gap that would
//! otherwise go undetected until live data hit production.
//!
//! # Scope
//!
//! The two `#[ignore]` tests drive the `WriterActor` directly via its
//! mpsc — they do NOT spin up the full R-02 NATS relay (that exercises
//! Plans 05/06/07's session-level orchestration and belongs in a UAT
//! test). The MCAP output shape — channel topic, schema name, message
//! decodability — is the critical SC7 assertion and is fully covered
//! here.
//!
//! The `cross_crate_compressed_video_wire_compat` test is pure
//! in-memory — no Postgres, no NATS, no Docker — so it runs without
//! `#[ignore]` and catches wire-compat regressions on every
//! `cargo test --features test-helpers` invocation.
//!
//! # Gate
//!
//! `#[cfg(feature = "test-helpers")]` per the 26.3 / 26.4 integration-test
//! convention. The two integration tests also carry `#[ignore = ...]`
//! because they need testcontainers. Run with:
//!
//! ```text
//! # Full suite including ignored integration tests (requires Docker):
//! cargo test -p roz-server --test mcap_camera_roundtrip \
//!     --features test-helpers -- --ignored
//!
//! # Just the non-ignored wire-compat test (no Docker required):
//! cargo test -p roz-server --test mcap_camera_roundtrip \
//!     --features test-helpers cross_crate_compressed_video_wire_compat
//! ```

#![cfg(feature = "test-helpers")]

use std::time::Duration;

use prost::Message as _;
use roz_core::camera::CameraId;
use roz_db::{create_pool, run_migrations};
use roz_server::observability::foxglove_types::foxglove;
use roz_server::observability::mcap_archive::{ChannelKey, FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::schema_registry::SchemaDescriptors;
use tempfile::TempDir;
use uuid::Uuid;

/// Build a minimal Annex-B H.264 NAL unit. Not a real decodable frame;
/// we assert shape, not decodability. Each frame has a unique `seq` byte
/// so MCAP reads can distinguish them.
fn synthetic_nalus(seq: u8) -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, // Annex-B 4-byte start code
        0x65, // NAL header: IDR slice
        seq, 0xDE, 0xAD, 0xBE, 0xEF,
    ]
}

/// Build a `CompressedVideo` message with the given synthetic NALUs.
fn build_compressed_video(frame_id: &str, seq: u8) -> Vec<u8> {
    let ts_ns: u64 = 1_700_000_000_000_000_000 + u64::from(seq) * 33_000_000;
    let msg = foxglove::CompressedVideo {
        timestamp: Some(prost_types::Timestamp {
            seconds: i64::try_from(ts_ns / 1_000_000_000).unwrap_or(0),
            nanos: i32::try_from(ts_ns % 1_000_000_000).unwrap_or(0),
        }),
        frame_id: frame_id.to_string(),
        data: synthetic_nalus(seq),
        format: "h264".to_string(),
    };
    msg.encode_to_vec()
}

struct PgHarness {
    pool: sqlx::PgPool,
    _pg: roz_test::PgGuard,
}

/// Boot a testcontainers Postgres, run migrations, create a tenant row
/// pinned to the given id, and return a pool plus its owning guard.
async fn bootstrap_pg_with_tenant(tenant_id: Uuid, label: &str) -> PgHarness {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();

    let pool = create_pool(&url).await.expect("create pool");
    run_migrations(&pool).await.expect("run migrations");

    let slug = format!("{label}-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(&pool, label, &slug, "personal")
        .await
        .expect("create tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("pin tenant id");
    PgHarness { pool, _pg: guard }
}

#[tokio::test]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn camera_keyframes_roundtrip() {
    let tenant_id = Uuid::new_v4();
    let pg = bootstrap_pg_with_tenant(tenant_id, "sc7-cam").await;
    let pool = pg.pool.clone();

    let session_id = Uuid::new_v4();
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("descriptor load");

    let writer_tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    let cam_id = CameraId::new("test_cam");

    // Register the camera dynamically (D-13: without this, subsequent
    // Events for this camera would warn-drop at ChannelKey::resolve).
    writer_tx
        .send(WriteCommand::RegisterCamera {
            camera_id: cam_id.clone(),
        })
        .await
        .expect("send RegisterCamera");

    // Drive 3 keyframe messages.
    for seq in 0u8..3 {
        let bytes = build_compressed_video("test_cam", seq);
        let log_time_ns: u64 = 1_700_000_000_000_000_000 + u64::from(seq) * 33_000_000;
        writer_tx
            .send(WriteCommand::Event {
                channel: ChannelKey::Camera(cam_id.clone()),
                log_time_ns,
                publish_time_ns: log_time_ns,
                bytes,
            })
            .await
            .expect("send Event");
    }

    // Finalize the file.
    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("send Finalize");
    drop(writer_tx);

    // Poll briefly for the writer task to finish + DB row transition,
    // matching the export_roundtrip pattern.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    // Find the MCAP file.
    let mcap_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    assert!(mcap_path.exists(), "mcap file should exist at {mcap_path:?}");
    let bytes = std::fs::read(&mcap_path).expect("read mcap");

    // Inspect summary for channel + schema.
    let summary = mcap::Summary::read(&bytes)
        .expect("summary read ok")
        .expect("summary present (non-empty file)");
    let camera_channel = summary
        .channels
        .values()
        .find(|c| c.topic == "/roz/camera/test_cam")
        .expect("/roz/camera/test_cam channel present");
    let schema = camera_channel
        .schema
        .as_ref()
        .expect("schema attached to camera channel");
    // R-01 honored: schema is CompressedVideo, not CompressedImage.
    assert_eq!(
        schema.name, "foxglove.CompressedVideo",
        "R-01: H.264 channels must use CompressedVideo, not CompressedImage"
    );
    assert_eq!(schema.encoding, "protobuf");

    // Iterate messages + decode.
    let mut camera_msg_count = 0;
    let stream = mcap::MessageStream::new(&bytes).expect("message stream");
    for result in stream {
        let msg = result.expect("message decode");
        if msg.channel.topic == "/roz/camera/test_cam" {
            camera_msg_count += 1;
            let decoded = foxglove::CompressedVideo::decode(msg.data.as_ref()).expect("CompressedVideo prost decode");
            assert_eq!(decoded.format, "h264", "format must be h264");
            assert_eq!(decoded.frame_id, "test_cam", "frame_id must be the camera id");
            assert!(!decoded.data.is_empty(), "data must be non-empty NALs");
            assert_eq!(
                &decoded.data[..4],
                &[0x00, 0x00, 0x00, 0x01],
                "Annex-B start code preserved verbatim"
            );
        }
    }
    assert_eq!(
        camera_msg_count, 3,
        "expected exactly 3 camera messages, saw {camera_msg_count}"
    );
}

#[tokio::test]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn unknown_camera_event_is_dropped_without_killing_actor() {
    let tenant_id = Uuid::new_v4();
    let pg = bootstrap_pg_with_tenant(tenant_id, "sc7-drop").await;
    let pool = pg.pool.clone();

    let session_id = Uuid::new_v4();
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("descriptor load");

    let writer_tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // 1. Send an Event for a camera that has NOT been RegisterCamera'd.
    //    ChannelKey::resolve returns None → the WriterActor logs a warn!
    //    and drops the frame (D-13). The actor does NOT exit.
    writer_tx
        .send(WriteCommand::Event {
            channel: ChannelKey::Camera(CameraId::new("never_registered")),
            log_time_ns: 1,
            publish_time_ns: 1,
            bytes: build_compressed_video("never_registered", 0),
        })
        .await
        .expect("send rogue Event");

    // 2. Now register a different camera and send 1 frame for it.
    //    If the actor had exited above, this send would fail.
    let cam_id = CameraId::new("real_cam");
    writer_tx
        .send(WriteCommand::RegisterCamera {
            camera_id: cam_id.clone(),
        })
        .await
        .expect("send RegisterCamera");
    writer_tx
        .send(WriteCommand::Event {
            channel: ChannelKey::Camera(cam_id.clone()),
            log_time_ns: 2,
            publish_time_ns: 2,
            bytes: build_compressed_video("real_cam", 0),
        })
        .await
        .expect("send real Event");

    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("send Finalize");
    drop(writer_tx);

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    let mcap_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    let bytes = std::fs::read(&mcap_path).expect("read mcap");
    let summary = mcap::Summary::read(&bytes)
        .expect("summary read")
        .expect("summary present");

    // real_cam channel is present:
    assert!(
        summary.channels.values().any(|c| c.topic == "/roz/camera/real_cam"),
        "/roz/camera/real_cam channel should exist after RegisterCamera"
    );
    // never_registered channel was NEVER registered; the D-13 drop path
    // never calls add_channel for it:
    assert!(
        !summary
            .channels
            .values()
            .any(|c| c.topic == "/roz/camera/never_registered"),
        "/roz/camera/never_registered must NOT be registered; Event was dropped at resolve() -> None"
    );

    // Exactly 1 real_cam message lands in the MCAP.
    let mut real_cam_count = 0;
    let stream = mcap::MessageStream::new(&bytes).expect("stream");
    for result in stream {
        let msg = result.expect("decode");
        if msg.channel.topic == "/roz/camera/real_cam" {
            real_cam_count += 1;
        }
    }
    assert_eq!(real_cam_count, 1, "expected 1 real_cam message, saw {real_cam_count}");
}

/// Phase 26.5 revision 26.5-warning-02: cross-crate wire-compat between
/// the worker's hand-vendored `CompressedVideo` and the server's
/// tonic-generated `CompressedVideo`. Silent field-tag drift (e.g.
/// `timestamp=1` on server vs `timestamp=2` on worker) would corrupt
/// live data without this check — neither plan's individual unit tests
/// exercise both structs at once.
///
/// No Docker, no Postgres, no NATS — pure in-memory. Runs on every
/// `cargo test --features test-helpers` invocation.
#[test]
fn cross_crate_compressed_video_wire_compat() {
    // 1. Construct the worker-side hand-vendored struct (Plan 05 declares
    //    this `pub struct` with all `pub` fields — verified in
    //    crates/roz-worker/src/camera/mcap_relay.rs).
    let worker_msg = roz_worker::camera::mcap_relay::CompressedVideo {
        timestamp: Some(prost_types::Timestamp {
            seconds: 1_700_000_001,
            nanos: 250_000_000,
        }),
        frame_id: "cross_crate_cam".to_string(),
        data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC, 0xDD],
        format: "h264".to_string(),
    };

    // 2. Encode via the worker's struct.
    let bytes = worker_msg.encode_to_vec();

    // 3. Decode via the server-side tonic-generated struct.
    let server_msg = foxglove::CompressedVideo::decode(bytes.as_slice())
        .expect("server-side CompressedVideo must decode worker-side encoded bytes");

    // 4. Assert every field matches by value. Any tag-number mismatch
    //    between the two copies would corrupt at least one field here.
    assert_eq!(
        server_msg.timestamp,
        Some(prost_types::Timestamp {
            seconds: 1_700_000_001,
            nanos: 250_000_000,
        }),
        "timestamp tag=1 must match between worker and server copies"
    );
    assert_eq!(server_msg.frame_id, "cross_crate_cam", "frame_id tag=2 must match");
    assert_eq!(
        server_msg.data,
        vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC, 0xDD],
        "data tag=3 must match"
    );
    assert_eq!(server_msg.format, "h264", "format tag=4 must match");

    // 5. Reverse direction — decode server-encoded bytes via the worker
    //    struct. Covers the opposite flow (e.g., if the server ever
    //    encodes CompressedVideo snapshots and the worker decodes them
    //    for diagnostics). Cheap insurance against one-directional drift.
    let server_bytes = server_msg.encode_to_vec();
    let worker_decoded = roz_worker::camera::mcap_relay::CompressedVideo::decode(server_bytes.as_slice())
        .expect("worker-side CompressedVideo must decode server-side encoded bytes");
    assert_eq!(worker_decoded.timestamp, worker_msg.timestamp);
    assert_eq!(worker_decoded.frame_id, worker_msg.frame_id);
    assert_eq!(worker_decoded.data, worker_msg.data);
    assert_eq!(worker_decoded.format, worker_msg.format);
}
