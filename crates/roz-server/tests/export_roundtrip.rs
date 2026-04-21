//! Phase 26 OBS-03 SC5: 30-second scripted session fixture round-trip.
//!
//! Drives a per-session [`WriterActor`] end-to-end with:
//!   - 1500 telemetry frames (50 Hz × 30 s) → `/tf` + `/roz/telemetry/pose`
//!   - 60 tool-call events (20 triplets) → `/roz/tool/calls`
//!   - 10 approval events (5 pairs) → `/roz/session/events` + `/roz/log`
//!     (driven through [`emit_session_event_for_tests`] — the INTEGRATION
//!     PATH — so a revert of `encode_session_event_proto` to the
//!     iteration-2 `None`-stub regression class would make this test fail)
//!   - 20 task-lifecycle placeholder events → `/roz/task/lifecycle`
//!
//! After [`WriteCommand::Finalize`], re-reads the MCAP via
//! [`mcap::MessageStream`] and asserts:
//!   - Total message count == 3100 (see fixture totals below)
//!   - `/roz/session/events` count > 0 AND == `APPROVAL_PAIRS * 2`
//!   - `/roz/log` count >= `APPROVAL_PAIRS * 2`
//!   - `/tf` and `/roz/telemetry/pose` topics are present
//!   - First `/tf` message decodes to [`FrameTransform`] with non-identity
//!     quaternion components matching the fixture (90° about z)
//!   - `roz_session_mcap_archives` row transitions to `finalized` with
//!     non-null digest + size
//!
//! Uses testcontainers for Postgres. NATS is NOT required — the fixture
//! writes telemetry directly via [`WriteCommand`] (bypassing the NATS
//! subscribe step, which is covered separately by cloud/edge parity
//! integration tests); approvals flow through the production converter
//! path via [`emit_session_event_for_tests`].
//!
//! Run with:
//!   ```text
//!   cargo test -p roz-server --features test-helpers \
//!       --test export_roundtrip -- --ignored --test-threads=1
//!   ```

#![allow(
    clippy::too_many_lines,
    reason = "integration test carries unavoidable fixture scaffolding"
)]

use mcap::MessageStream;
use prost::Message as _;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::session::feedback::ApprovalOutcome;
use roz_db::{create_pool, run_migrations};
use roz_server::observability::ingest_cloud::emit_session_event_for_tests;
use roz_server::observability::mcap_archive::{ChannelKey, FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::projection::{
    FrameTransform, LogLevel, PoseInFrame, Vector3, copper_quat_to_foxglove, log_line, ns_to_proto_timestamp,
    pose_in_frame,
};
use roz_server::observability::schema_registry::SchemaDescriptors;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture constants (D-10 + OBS-03 acceptance).
// ---------------------------------------------------------------------------

const SESSION_DURATION_SECS: u64 = 30;
const TELEMETRY_HZ: u64 = 50;
const TOTAL_TELEMETRY: u64 = SESSION_DURATION_SECS * TELEMETRY_HZ; // 1500
const TOOL_CALL_TRIPLETS: u64 = 20;
const APPROVAL_PAIRS: u64 = 5;
const TASK_LIFECYCLE_TRANSITIONS: u64 = 20;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires testcontainers Postgres + --features test-helpers"]
async fn sc5_30s_fixture_roundtrips_via_mcap_message_stream() {
    // 1. Testcontainers Postgres + migrations + pool.
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard); // keep container alive past function scope
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    // 2. Create a tenant row (schema requires name + slug). Pin the tenant_id
    //    we want via a post-create UPDATE so the archive row's tenant_id
    //    matches what we pass to `spawn_writer`.
    let tenant_id = Uuid::new_v4();
    let slug = format!("sc5-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(&pool, "SC5 Test", &slug, "personal")
        .await
        .expect("create tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("update tenant id");

    // 3. Tempdir for MCAP output; load schema descriptors up-front.
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("descriptor load");

    // 4. Spawn a WriterActor for this session.
    let session_id = Uuid::new_v4();
    let tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // 5. Drive fixture messages.
    let now_ns: u64 = 1_700_000_000_000_000_000; // fixed epoch for determinism

    // --- 1500 telemetry frames: /tf (FrameTransform) + /roz/telemetry/pose.
    //
    // Direct WriteCommand injection is acceptable here because the NATS
    // telemetry pipeline is not under test in this fixture (it is covered
    // separately by cloud/edge parity integration tests). Session events go
    // through the production converter (below).
    for i in 0..TOTAL_TELEMETRY {
        let ts = now_ns + i * 20_000_000; // 50 Hz = 20 ms spacing
        let ft = fixture_frame_transform(ts);
        let mut buf = Vec::new();
        ft.encode(&mut buf).expect("encode FrameTransform");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::Tf,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /tf");

        let pose = fixture_pose(ts);
        let mut buf = Vec::new();
        pose.encode(&mut buf).expect("encode PoseInFrame");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::Pose,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /roz/telemetry/pose");
    }

    // --- 60 tool-call events (3 per triplet: Started / Requested / Finished).
    //    The real `ToolCallEvent` envelope is exercised elsewhere; here we
    //    use a `log_line` stub as a message-count fallback because this
    //    fixture's contract is shape-correct (count + channel presence),
    //    not payload-correct, for tool calls.
    for i in 0..TOOL_CALL_TRIPLETS {
        let ts = now_ns + i * 1_500_000_000;
        for variant in 0..3u8 {
            let stub = log_line(LogLevel::Info, ts, "tool", &format!("tool_{i}_v{variant}"));
            let mut buf = Vec::new();
            stub.encode(&mut buf).expect("encode tool call stub");
            tx.send(WriteCommand::Event {
                channel: ChannelKey::ToolCalls,
                log_time_ns: ts,
                publish_time_ns: ts,
                bytes: buf,
            })
            .await
            .expect("send /roz/tool/calls");
        }
    }

    // --- 10 approval events (2 per pair) — DRIVEN THROUGH THE INTEGRATION
    //     PATH. This is the SC5 anti-regression guard. Each call flows:
    //
    //       EventEnvelope
    //         → emit_session_event_for_tests          (Plan 26-11 Task 1)
    //         → ingest_cloud::emit_session_event      (pub(crate))
    //         → ingest_cloud::encode_session_event_proto
    //         → event_mapper::event_envelope_to_session_response
    //         → WriteCommand::Event { channel: ChannelKey::SessionEvents,
    //                                 bytes: prost_bytes }
    //
    //     A regression that reverts `encode_session_event_proto` to
    //     `return None` would leave `/roz/session/events` empty, so
    //     `session_events_count == 0` and the assertion below would fail.
    for i in 0..APPROVAL_PAIRS {
        for variant in 0..2u8 {
            let envelope = EventEnvelope {
                event_id: EventId::new(),
                correlation_id: CorrelationId::new(),
                parent_event_id: None,
                timestamp: chrono::Utc::now(),
                event: if variant == 0 {
                    SessionEvent::ApprovalRequested {
                        approval_id: format!("approval_{i}"),
                        action: "tool:shell".into(),
                        reason: format!("approval request {i}"),
                        timeout_secs: 300,
                    }
                } else {
                    SessionEvent::ApprovalResolved {
                        approval_id: format!("approval_{i}"),
                        outcome: ApprovalOutcome::Approved,
                    }
                },
            };
            emit_session_event_for_tests(&tx, &envelope).await;
        }
    }

    // --- 20 task lifecycle transitions (stubbed as log lines; see tool
    //     calls note above — shape-correct, not payload-correct).
    for i in 0..TASK_LIFECYCLE_TRANSITIONS {
        let ts = now_ns + i * 1_500_000_000;
        let stub = log_line(LogLevel::Info, ts, "task", &format!("task_transition_{i}"));
        let mut buf = Vec::new();
        stub.encode(&mut buf).expect("encode task lifecycle stub");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::TaskLifecycle,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /roz/task/lifecycle");
    }

    // 6. Finalize. Drop the sender so the WriterActor drain completes.
    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("send Finalize");
    drop(tx);
    // Poll briefly for the writer task to finish + DB row transition.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    // 7. Re-read the MCAP via MessageStream.
    let file_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    let data = std::fs::read(&file_path).expect("mcap file exists after finalize");

    let mut count: u64 = 0;
    let mut session_events_count: u64 = 0;
    let mut log_count: u64 = 0;
    let mut seen_tf = false;
    let mut seen_pose = false;
    let mut tf_non_identity: Option<FrameTransform> = None;
    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        match msg.channel.topic.as_str() {
            "/tf" => {
                seen_tf = true;
                if tf_non_identity.is_none() {
                    tf_non_identity = FrameTransform::decode(&*msg.data).ok();
                }
            }
            "/roz/telemetry/pose" => {
                seen_pose = true;
            }
            "/roz/session/events" => {
                session_events_count += 1;
            }
            "/roz/log" => {
                log_count += 1;
            }
            _ => {}
        }
        count += 1;
    }

    // /roz/session/events anti-regression assertion — see approval-loop
    // comment. A revert of `encode_session_event_proto` to a None-stub would
    // make this fail.
    assert!(
        session_events_count > 0,
        "/roz/session/events must receive messages via the integration path; \
         got 0, which indicates encode_session_event_proto regressed to None \
         or the converter path broke (see Plan 26-05 Task 2)",
    );

    // Exact count: 5 approval pairs × 2 events = 10 on /roz/session/events.
    let expected_session_events = APPROVAL_PAIRS * 2;
    assert_eq!(
        session_events_count, expected_session_events,
        "expected {expected_session_events} /roz/session/events messages, got {session_events_count}",
    );

    // /roz/log receives the approval summaries too (plus anything else
    // that routes through emit_session_event). Lower-bound the count.
    assert!(
        log_count >= expected_session_events,
        "/roz/log must receive at least one line per session event; \
         got {log_count} (expected >= {expected_session_events})",
    );

    // Fixture totals:
    //   telemetry: 1500 * 2 = 3000   (/tf + /roz/telemetry/pose)
    //   tool calls: 20 * 3 = 60      (/roz/tool/calls)
    //   approvals: 5 * 2 = 10 session_events + 10 log = 20
    //   task lifecycle: 20           (/roz/task/lifecycle)
    //   TOTAL: 3100
    let expected = TOTAL_TELEMETRY * 2
        + TOOL_CALL_TRIPLETS * 3
        + APPROVAL_PAIRS * 2 * 2 // each approval → 1 /roz/log + 1 /roz/session/events
        + TASK_LIFECYCLE_TRANSITIONS;
    assert_eq!(count, expected, "message count must match fixture totals");
    assert!(seen_tf, "/tf channel present");
    assert!(seen_pose, "/roz/telemetry/pose channel present");

    // Non-identity quaternion round-trip verification — RESEARCH Pitfall 2.
    // Fixture quat is 90° about z → copper [w,x,y,z] = [sqrt(1/2), 0, 0, sqrt(1/2)]
    // → foxglove {x, y, z, w} = {0, 0, sqrt(1/2), sqrt(1/2)}.
    let ft = tf_non_identity.expect("at least one /tf message decoded");
    let q = ft.rotation.expect("rotation present");
    let half = std::f64::consts::FRAC_1_SQRT_2;
    assert!((q.x - 0.0).abs() < 1e-9, "quaternion x component (got {})", q.x);
    assert!((q.y - 0.0).abs() < 1e-9, "quaternion y component (got {})", q.y);
    assert!((q.z - half).abs() < 1e-9, "quaternion z component (got {})", q.z);
    assert!((q.w - half).abs() < 1e-9, "quaternion w component (got {})", q.w);

    // DB row transitioned to finalized with digest + size populated.
    let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
        .await
        .expect("db lookup");
    assert_eq!(rows.len(), 1, "expected exactly one archive row");
    assert_eq!(
        rows[0].status, "finalized",
        "status must be finalized after SessionCompleted"
    );
    assert!(
        rows[0].digest_sha256.is_some(),
        "digest_sha256 must be populated on finalize"
    );
    assert!(rows[0].size_bytes > 0, "size_bytes must be positive on finalize");
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a `FrameTransform` with a non-identity rotation (90° about z).
///
/// Copper's `[w, x, y, z]` convention: `[sqrt(1/2), 0, 0, sqrt(1/2)]`.
/// After `copper_quat_to_foxglove` reorder → foxglove `{x=0, y=0, z=sqrt(1/2), w=sqrt(1/2)}`.
fn fixture_frame_transform(timestamp_ns: u64) -> FrameTransform {
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let copper_wxyz = [half, 0.0, 0.0, half];
    FrameTransform {
        timestamp: Some(ns_to_proto_timestamp(timestamp_ns)),
        parent_frame_id: "world".into(),
        child_frame_id: "base_link".into(),
        translation: Some(Vector3 { x: 1.0, y: 2.0, z: 3.0 }),
        rotation: Some(copper_quat_to_foxglove(copper_wxyz)),
    }
}

fn fixture_pose(timestamp_ns: u64) -> PoseInFrame {
    let half = std::f64::consts::FRAC_1_SQRT_2;
    pose_in_frame("base_link", [1.0, 2.0, 3.0], [half, 0.0, 0.0, half], timestamp_ns)
}
