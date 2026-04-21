//! Phase 26: non-SC5 observability integration tests — completion finalize,
//! recovery smoke, cross-tenant deny, path traversal rejection.
//!
//! SC5 itself lives in [`export_roundtrip.rs`]; this file carries the
//! shorter integration suite the plan calls out (D-04 recovery, D-06
//! status vocabulary, tenant scoping, export-path safety).
//!
//! All tests are `#[ignore]` per workspace convention; run with:
//!   ```text
//!   cargo test -p roz-server --test observability_integration \
//!       -- --ignored --test-threads=1
//!   ```

#![allow(clippy::too_many_lines, reason = "integration tests carry unavoidable scaffolding")]

use prost::Message as _;
use roz_db::{create_pool, run_migrations};
use roz_server::observability::mcap_archive::{ChannelKey, FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::projection::{LogLevel, log_line};
use roz_server::observability::schema_registry::SchemaDescriptors;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared setup helpers
// ---------------------------------------------------------------------------

/// Spin up a testcontainers Postgres, run migrations, build a pool, and
/// allocate a tempdir for MCAP output. Returns the tempdir guard (drop to
/// clean up) + live pool + canonicalised mcap_dir path.
async fn setup() -> (TempDir, sqlx::PgPool, std::path::PathBuf) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard);
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    (tmp, pool, mcap_dir)
}

/// Seed a tenant row and pin its id. Necessary because
/// `roz_session_mcap_archives.tenant_id` is FK-constrained to
/// `roz_tenants.id`.
async fn seed_tenant(pool: &sqlx::PgPool, tenant_id: Uuid) {
    let slug = format!("obs-int-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(pool, "Obs Integration", &slug, "personal")
        .await
        .expect("create tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(pool)
        .await
        .expect("pin tenant id");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy-path D-06 transition: `status='open'` → `status='finalized'` on
/// `WriteCommand::Finalize { SessionCompleted }`.
#[tokio::test]
#[ignore = "requires testcontainers Postgres"]
async fn completion_finalizes_db_row_to_finalized() {
    let (_tmp, pool, mcap_dir) = setup().await;
    let tenant_id = Uuid::new_v4();
    seed_tenant(&pool, tenant_id).await;

    let session_id = Uuid::new_v4();
    let descriptors = SchemaDescriptors::load().expect("descriptor load");
    let tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // Write one event so the file is non-trivial.
    let log = log_line(LogLevel::Info, 1_700_000_000_000_000_000, "test", "hello");
    let mut buf = Vec::new();
    log.encode(&mut buf).expect("encode log");
    tx.send(WriteCommand::Event {
        channel: ChannelKey::Log,
        log_time_ns: 1_700_000_000_000_000_000,
        publish_time_ns: 1_700_000_000_000_000_000,
        bytes: buf,
    })
    .await
    .expect("send log event");

    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("send Finalize");
    drop(tx);

    // Poll briefly for the DB row to transition.
    let mut rows = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    assert_eq!(rows.len(), 1, "expected exactly one archive row");
    assert_eq!(
        rows[0].status, "finalized",
        "status must be 'finalized' after SessionCompleted"
    );
    assert!(
        rows[0].digest_sha256.is_some(),
        "digest_sha256 must be populated on finalize"
    );
    assert!(rows[0].size_bytes > 0, "size_bytes must be positive on finalize");
}

/// D-04 recovery smoke: a row left at `status='open'` with a truncated
/// partial file transitions to `status='recovered_incomplete'` with a
/// populated digest + size after `recover_all_open_archives` runs.
///
/// Executor note: the straightforward shutdown path (drop sender) cleanly
/// finalizes the writer, leaving `status='finalized'`. To exercise the
/// recovery path we synthesise a crashed state: manually UPDATE the row
/// back to `status='open'` + `digest_sha256=NULL` and truncate the file's
/// tail. `recover_all_open_archives` then reads it via
/// `mcap::read::Options::IgnoreEndMagic`, copies surviving records to a
/// fresh writer, and finalises the DB row.
#[tokio::test]
#[ignore = "requires testcontainers Postgres"]
async fn recovery_smoke_partial_file_transitions_to_recovered_incomplete() {
    let (_tmp, pool, mcap_dir) = setup().await;
    let tenant_id = Uuid::new_v4();
    seed_tenant(&pool, tenant_id).await;

    let session_id = Uuid::new_v4();
    let descriptors = SchemaDescriptors::load().expect("descriptor load");
    let tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // Write one event so the file has actual content.
    let log = log_line(LogLevel::Info, 1_700_000_000_000_000_000, "crash-test", "before crash");
    let mut buf = Vec::new();
    log.encode(&mut buf).expect("encode log");
    tx.send(WriteCommand::Event {
        channel: ChannelKey::Log,
        log_time_ns: 1_700_000_000_000_000_000,
        publish_time_ns: 1_700_000_000_000_000_000,
        bytes: buf,
    })
    .await
    .expect("send log event");

    // Finalize cleanly so the file exists on disk.
    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("send Finalize");
    drop(tx);
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    // Synthesise a crashed writer: reset the row back to 'open' and
    // truncate the tail of the file (drop the end magic).
    sqlx::query(
        "UPDATE roz_session_mcap_archives \
         SET status='open', digest_sha256=NULL, finalized_at=NULL \
         WHERE tenant_id=$1 AND session_id=$2",
    )
    .bind(tenant_id)
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("reset to open");

    let file_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    let orig = std::fs::metadata(&file_path).expect("file exists").len();
    // Drop last 20 bytes to simulate a truncated tail. MCAP end-magic
    // recovery uses `IgnoreEndMagic`.
    let truncated_len = orig.saturating_sub(20);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&file_path)
        .expect("open file rw");
    f.set_len(truncated_len).expect("truncate tail");

    // Run recovery.
    let recovered = roz_server::observability::recovery::recover_all_open_archives(&pool, &mcap_dir)
        .await
        .expect("recovery scan");
    assert!(
        recovered >= 1,
        "recovery must transition at least one row (got {recovered})"
    );

    // Verify the row is now 'recovered_incomplete' with digest + size.
    let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
        .await
        .expect("db lookup");
    let row = rows.first().expect("archive row still present");
    assert_eq!(
        row.status, "recovered_incomplete",
        "status must transition to 'recovered_incomplete' after recovery"
    );
    assert!(row.digest_sha256.is_some(), "digest_sha256 populated after recovery");
    assert!(row.size_bytes > 0, "size_bytes positive after recovery");
}

/// Cross-tenant export is denied. Scaffold: spins up
/// [`ObservabilityServiceImpl`] behind an AuthIdentity-injection middleware,
/// inserts an archive owned by tenant B, then issues an export request as
/// tenant A. Must receive a non-`Ok` status (`NotFound` is acceptable —
/// the in-DB filter doesn't leak existence, `PermissionDenied` is
/// defense-in-depth).
///
/// Full body left to the `observability_export_grpc.rs` harness (already
/// covers cross-tenant scenarios explicitly). This test is kept as a
/// named slot so coverage is enumerated in one place.
#[tokio::test]
#[ignore = "covered by observability_export_grpc.rs::cross_tenant_request_returns_not_found_without_existence_leak"]
async fn export_cross_tenant_denied() {
    // Intentional no-op: see doc comment.
}

/// Export rejects rows whose canonicalised `path` column escapes
/// `ROZ_MCAP_DIR`. Scaffold: insert an archive row with a path under
/// `/tmp/` (outside mcap_dir), then call the export handler and assert
/// the error status.
///
/// Full body left to the `observability_export_grpc.rs` harness
/// (`path_outside_mcap_root_returns_internal`). Kept here as a named
/// slot so Phase 26's coverage surface is declared in one place.
#[tokio::test]
#[ignore = "covered by observability_export_grpc.rs::path_outside_mcap_root_returns_internal"]
async fn export_rejects_path_traversal_outside_mcap_dir() {
    // Intentional no-op: see doc comment.
}
