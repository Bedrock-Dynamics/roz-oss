//! Phase 26.4 SC7: multi-session fleet-query regression.
//!
//! Seeds 5 distinct sessions into the MCAP + DB pipeline with varying
//! tool-call and intervention counts, then runs the canonical fleet query
//!
//! ```sql
//! SELECT session_id FROM roz_session_metadata
//!  WHERE tool_call_count > 50 AND intervention_count > 0
//! ```
//!
//! and asserts the returned session set matches the 3 predicted rows.
//!
//! Also exercises D-32 idempotency: `index_session` called twice on the
//! same session leaves row counts unchanged.
//!
//! Seeded session matrix:
//!
//! | Session | tool_calls | interventions | Matches predicate? |
//! |---------|------------|---------------|--------------------|
//! | A       | 60         | 2             | YES                |
//! | B       | 75         | 1             | YES                |
//! | C       | 51         | 3             | YES                |
//! | D       | 60         | 0             | no (no interventions) |
//! | E       | 20         | 5             | no (too few tool calls) |
//!
//! Expected fleet-query result: {A, B, C}.
//!
//! Gated behind `test-helpers`; run with:
//! ```text
//! cargo test -p roz-server --features test-helpers \
//!     --test session_metadata_fleet_query -- --ignored --test-threads=1
//! ```

#![cfg(feature = "test-helpers")]
#![allow(
    clippy::too_many_lines,
    reason = "integration test carries unavoidable fixture scaffolding"
)]

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use roz_core::controller::intervention::InterventionKind;
use roz_core::session::SessionUsage;
use roz_core::session::control::SessionMode;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_db::{create_pool, run_migrations};
use roz_server::observability::ingest_cloud::emit_session_event_for_tests;
// NOTE: the channel-key constructor and std Arc are deliberately NOT imported —
// this test does NOT construct `WriteCommand::Event` directly; it drives events
// through `emit_session_event_for_tests`, which handles channel keying
// internally. Importing either would trip `cargo clippy -- -D warnings` on
// unused imports (plan-checker regression gate).
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::metadata_index::index_session;
use roz_server::observability::schema_registry::SchemaDescriptors;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Seed one session: spawn a [`spawn_writer`] actor, emit `tool_calls`
/// `ToolCall{Requested,Started,Finished}` triplets plus `interventions`
/// [`SessionEvent::SafetyIntervention`] events, then finalize.
///
/// Returns the session UUID after the archive row transitions to
/// `finalized` AND the spawned `index_session` call has written the
/// metadata row (polled with a bounded deadline).
async fn seed_session(
    pool: &PgPool,
    tenant_id: Uuid,
    mcap_dir: &Path,
    descriptors: SchemaDescriptors,
    tool_calls: u32,
    interventions: u32,
) -> Uuid {
    let session_id = Uuid::new_v4();
    let tx = spawn_writer(
        mcap_dir.to_path_buf(),
        tenant_id,
        session_id,
        descriptors,
        pool.clone(),
        None,
    )
    .await
    .expect("spawn_writer");

    // 1. SessionStarted — captures started_at.
    send_event(
        &tx,
        SessionEvent::SessionStarted {
            session_id: session_id.to_string(),
            mode: SessionMode::Local,
            blueprint_version: "test".into(),
            model_name: Some("mock".into()),
            permissions: vec![],
        },
    )
    .await;

    // 2. TurnStarted — bumps turn_count so the metadata row is non-trivial.
    send_event(&tx, SessionEvent::TurnStarted { turn_index: 0 }).await;

    // 3. N tool-call triplets (Requested → Started → Finished for the same call_id).
    for i in 0..tool_calls {
        let call_id = format!("mock_call_{i}");
        let tool_name = format!("mock_tool_{i}");
        send_event(
            &tx,
            SessionEvent::ToolCallRequested {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                parameters: serde_json::json!({}),
                timeout_ms: 5_000,
            },
        )
        .await;
        send_event(
            &tx,
            SessionEvent::ToolCallStarted {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                category: "pure".into(),
            },
        )
        .await;
        send_event(
            &tx,
            SessionEvent::ToolCallFinished {
                call_id,
                tool_name,
                result_summary: "ok".into(),
            },
        )
        .await;
    }

    // 4. M SafetyIntervention events — drives intervention_count.
    for i in 0..interventions {
        send_event(
            &tx,
            SessionEvent::SafetyIntervention {
                channel: format!("ch_{i}"),
                raw_value: 1.0,
                clamped_value: 0.5,
                kind: InterventionKind::VelocityClamp,
                reason: "test intervention".into(),
            },
        )
        .await;
    }

    // 5. SessionCompleted — terminal event → outcome = "succeeded".
    send_event(
        &tx,
        SessionEvent::SessionCompleted {
            summary: "ok".into(),
            total_usage: SessionUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
        },
    )
    .await;

    // 6. Finalize terminally — triggers the spawned `index_session` at the
    //    spawn site inside `WriterActor::finalize_file` (CONTEXT D-07).
    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("finalize");
    drop(tx);

    // Poll for the archive row transition + the detached indexer to write
    // its row. Bounded deadline: 50 × 100 ms = 5 s.
    for _ in 0..50 {
        let archives = roz_db::mcap_archives::list_by_session(pool, tenant_id, session_id)
            .await
            .expect("list_by_session");
        let finalized = archives.iter().any(|r| r.status == "finalized");
        let indexed = roz_db::session_metadata::fetch_metadata(pool, session_id)
            .await
            .expect("fetch_metadata")
            .is_some();
        if finalized && indexed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    session_id
}

/// Build an `EventEnvelope` carrying the given [`SessionEvent`] and drive it
/// through the production converter path via [`emit_session_event_for_tests`].
async fn send_event(tx: &mpsc::Sender<WriteCommand>, event: SessionEvent) {
    let envelope = EventEnvelope {
        event_id: EventId::new(),
        correlation_id: CorrelationId::new(),
        parent_event_id: None,
        timestamp: chrono::Utc::now(),
        event,
        trace_id: None,
        span_id: None,
    };
    emit_session_event_for_tests(tx, &envelope).await;
}

#[tokio::test]
#[ignore = "requires testcontainers + --features test-helpers"]
async fn fleet_query_returns_sessions_matching_predicate() {
    // ---- Setup: testcontainer Postgres + migrations + pool ----
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    // Keep the container alive past function scope — the WriterActor's
    // detached `index_session` spawn may still be running when the test
    // function returns.
    std::mem::forget(guard);
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrations");

    // ---- Tenant seed ----
    let tenant = roz_db::tenant::create_tenant(&pool, "fleet-test", &format!("fleet-{}", Uuid::new_v4()), "personal")
        .await
        .expect("create tenant");

    // ---- Per-session MCAP output + schema descriptors ----
    let mcap_root = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(mcap_root.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("load descriptors");

    // ---- Seed 5 sessions ----
    //   A: 60 tool-calls, 2 interventions  → matches
    //   B: 75 tool-calls, 1 intervention   → matches
    //   C: 51 tool-calls, 3 interventions  → matches
    //   D: 60 tool-calls, 0 interventions  → no (intervention_count = 0)
    //   E: 20 tool-calls, 5 interventions  → no (tool_call_count <= 50)
    let session_a = seed_session(&pool, tenant.id, &mcap_dir, descriptors.clone(), 60, 2).await;
    let session_b = seed_session(&pool, tenant.id, &mcap_dir, descriptors.clone(), 75, 1).await;
    let session_c = seed_session(&pool, tenant.id, &mcap_dir, descriptors.clone(), 51, 3).await;
    let session_d = seed_session(&pool, tenant.id, &mcap_dir, descriptors.clone(), 60, 0).await;
    let session_e = seed_session(&pool, tenant.id, &mcap_dir, descriptors.clone(), 20, 5).await;

    // ---- Run the canonical SC7 fleet query ----
    let matching: Vec<Uuid> = sqlx::query_scalar(
        "SELECT session_id FROM roz_session_metadata \
          WHERE tool_call_count > 50 AND intervention_count > 0",
    )
    .fetch_all(&pool)
    .await
    .expect("fleet query");

    let matching_set: HashSet<Uuid> = matching.into_iter().collect();
    assert!(matching_set.contains(&session_a), "A (60 tc, 2 iv) must match");
    assert!(matching_set.contains(&session_b), "B (75 tc, 1 iv) must match");
    assert!(matching_set.contains(&session_c), "C (51 tc, 3 iv) must match");
    assert!(!matching_set.contains(&session_d), "D (60 tc, 0 iv) must NOT match");
    assert!(!matching_set.contains(&session_e), "E (20 tc, 5 iv) must NOT match");

    // ---- D-32 idempotency: re-index session A; row counts must stay constant ----
    let tool_rows_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_tool_calls WHERE session_id = $1")
        .bind(session_a)
        .fetch_one(&pool)
        .await
        .expect("count tool-call rows before reindex");
    let metadata_rows_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_metadata WHERE session_id = $1")
            .bind(session_a)
            .fetch_one(&pool)
            .await
            .expect("count metadata rows before reindex");
    let indexed_at_before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT indexed_at FROM roz_session_metadata WHERE session_id = $1")
            .bind(session_a)
            .fetch_one(&pool)
            .await
            .expect("indexed_at before reindex");

    let _reindex = index_session(&pool, tenant.id, session_a)
        .await
        .expect("reindex session A");

    let tool_rows_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_tool_calls WHERE session_id = $1")
        .bind(session_a)
        .fetch_one(&pool)
        .await
        .expect("count tool-call rows after reindex");
    let metadata_rows_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_metadata WHERE session_id = $1")
            .bind(session_a)
            .fetch_one(&pool)
            .await
            .expect("count metadata rows after reindex");
    let indexed_at_after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT indexed_at FROM roz_session_metadata WHERE session_id = $1")
            .bind(session_a)
            .fetch_one(&pool)
            .await
            .expect("indexed_at after reindex");

    assert_eq!(
        tool_rows_before, tool_rows_after,
        "D-32 idempotency: tool-call row count must not change on reindex"
    );
    assert_eq!(
        metadata_rows_before, metadata_rows_after,
        "D-32 idempotency: metadata row count must stay 1 on reindex"
    );
    assert_eq!(
        metadata_rows_after, 1,
        "metadata row count per session must be exactly 1"
    );
    assert!(
        indexed_at_after >= indexed_at_before,
        "D-32: indexed_at must be refreshed (or equal if clock tick resolution) on reindex; \
         before={indexed_at_before}, after={indexed_at_after}"
    );
}
