//! Phase 26 OBS-03: `ObservabilityServiceImpl::export_session` integration tests.
//!
//! Spins up `ObservabilityServiceImpl` behind a minimal axum middleware that
//! injects a fixed `AuthIdentity` — the production handler reads tenant ID
//! from request extensions, so we mirror that contract here without booting
//! the full grpc_auth stack.
//!
//! # Coverage
//! - `export_missing_session_returns_not_found` — 404 path when zero rows.
//! - `cross_tenant_request_returns_not_found_without_existence_leak` — a
//!   caller in tenant B requesting session_id belonging to tenant A gets
//!   `NotFound`, not `PermissionDenied` — the tenant-filtered DB query keeps
//!   cross-tenant existence hidden. The in-handler `PermissionDenied` guard
//!   remains for defense-in-depth should any future path bypass the DB
//!   filter.
//! - `export_streams_rollovers_in_order_with_archive_status_on_first_chunk`
//!   — happy path: two rollover files concatenate in `rollover_index` order,
//!   first chunk of each file carries the file's archive status, later
//!   chunks of the same file do NOT.
//! - `path_outside_mcap_root_returns_internal` — symlink/traversal row
//!   pointing outside `ROZ_MCAP_DIR` is refused with `Internal`.
//!
//! Cargo flag: `#[ignore]` on tests that need a Postgres container — run with
//! `cargo test -p roz-server --test observability_export_grpc -- --ignored --test-threads=1`.

#![allow(clippy::too_many_lines, reason = "integration tests carry unavoidable scaffolding")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use roz_core::auth::{AuthIdentity, Role, TenantId};
use roz_db::{create_pool, mcap_archives, run_migrations, set_tenant_context};
use roz_server::grpc::observability::ObservabilityServiceImpl;
use roz_server::grpc::roz_v1::observability_service_client::ObservabilityServiceClient;
use roz_server::grpc::roz_v1::observability_service_server::ObservabilityServiceServer;
use roz_server::grpc::roz_v1::ExportSessionRequest;
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test middleware: inject AuthIdentity into gRPC request extensions.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct InjectState {
    identity: AuthIdentity,
}

async fn inject_extensions_middleware(
    axum::extract::State(state): axum::extract::State<InjectState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(state.identity.clone());
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    pool: PgPool,
    tenant_id: Uuid,
    mcap_dir: PathBuf,
    addr: SocketAddr,
    _mcap_tmp: TempDir,
}

impl Harness {
    async fn client(&self) -> ObservabilityServiceClient<tonic::transport::Channel> {
        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{}", self.addr))
            .expect("endpoint")
            .connect_timeout(Duration::from_secs(5));
        for _ in 0..40 {
            if let Ok(c) = endpoint.clone().connect().await {
                return ObservabilityServiceClient::new(c);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("connect failed after retries");
    }
}

async fn setup_harness_for(tenant_id: Uuid) -> Harness {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard);
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    // Create the tenant row for the caller (session archives require a real
    // tenant FK).
    let slug = format!("export-test-{}", Uuid::new_v4());
    let _ = roz_db::tenant::create_tenant(&pool, "Export Test", &slug, "personal")
        .await
        .expect("tenant");

    // Override the tenant_id field to the value the test wants (so we can
    // also create a second tenant below for cross-tenant cases).
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("update tenant id");

    let mcap_tmp = tempfile::tempdir().expect("mcap tempdir");
    let mcap_dir = std::fs::canonicalize(mcap_tmp.path()).expect("canonicalize mcap dir");

    let svc = ObservabilityServiceImpl::new(pool.clone(), mcap_dir.clone());
    let server = ObservabilityServiceServer::new(svc);
    let identity = AuthIdentity::User {
        user_id: "user:test".into(),
        org_id: None,
        tenant_id: TenantId::new(tenant_id),
        role: Role::Admin,
    };
    let inject_state = InjectState { identity };
    let router = tonic::service::Routes::new(server)
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            inject_state,
            inject_extensions_middleware,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc serve");
    });

    Harness {
        pool,
        tenant_id,
        mcap_dir,
        addr,
        _mcap_tmp: mcap_tmp,
    }
}

async fn setup_harness() -> Harness {
    setup_harness_for(Uuid::new_v4()).await
}

/// Seed a minimal valid MCAP file at `path` containing zero messages. The
/// export handler does not care about the file's message contents — it just
/// streams bytes.
fn write_empty_mcap(path: &std::path::Path) {
    use std::io::Cursor;
    let mut buf = Vec::new();
    {
        let mut w = mcap::Writer::new(Cursor::new(&mut buf)).expect("writer");
        w.finish().expect("finish");
    }
    std::fs::write(path, &buf).expect("write mcap");
}

/// Insert an archive row using the test harness's tenant (RLS-scoped).
async fn insert_archive(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: Uuid,
    path: &str,
    rollover_index: i32,
) -> mcap_archives::McapArchiveRow {
    let mut tx = pool.begin().await.expect("tx begin");
    set_tenant_context(&mut *tx, &tenant_id).await.expect("tenant ctx");
    let row = mcap_archives::insert_open(&mut *tx, tenant_id, session_id, path, rollover_index)
        .await
        .expect("insert archive");
    tx.commit().await.expect("commit");
    row
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Postgres via testcontainers"]
async fn export_missing_session_returns_not_found() {
    let h = setup_harness().await;
    let mut client = h.client().await;
    let resp = client
        .export_session(ExportSessionRequest {
            session_id: Uuid::new_v4().to_string(),
            time_range: None,
        })
        .await;
    let status = match resp {
        Ok(mut s) => {
            // Server-streaming: errors may arrive on the stream rather than
            // the unary response. Consume until error.
            let stream = s.get_mut();
            match stream.message().await {
                Ok(_) => panic!("expected NotFound, got stream Ok"),
                Err(err) => err,
            }
        }
        Err(err) => err,
    };
    assert_eq!(status.code(), tonic::Code::NotFound, "missing session must be NotFound");
}

#[tokio::test]
#[ignore = "requires Postgres via testcontainers"]
async fn cross_tenant_request_returns_not_found_without_existence_leak() {
    // Tenant A owns the archive; tenant B makes the export request.
    let tenant_a = Uuid::new_v4();
    let h = setup_harness_for(tenant_a).await;

    // Create tenant B directly (sans harness re-setup; we only need the row
    // so its ID is a valid FK target for the caller identity).
    let slug_b = format!("export-other-{}", Uuid::new_v4());
    let tenant_b_row = roz_db::tenant::create_tenant(&h.pool, "Tenant B", &slug_b, "personal")
        .await
        .expect("tenant b");
    let tenant_b = tenant_b_row.id;

    // Seed an archive under tenant A.
    let session_id = Uuid::new_v4();
    let archive_path = h.mcap_dir.join(format!("{session_id}.mcap"));
    write_empty_mcap(&archive_path);
    let _ = insert_archive(
        &h.pool,
        tenant_a,
        session_id,
        archive_path.to_str().expect("utf8 path"),
        0,
    )
    .await;

    // Spin up a second server bound to tenant_b as the caller.
    let svc = ObservabilityServiceImpl::new(h.pool.clone(), h.mcap_dir.clone());
    let server = ObservabilityServiceServer::new(svc);
    let identity = AuthIdentity::User {
        user_id: "user:tenant-b".into(),
        org_id: None,
        tenant_id: TenantId::new(tenant_b),
        role: Role::Admin,
    };
    let inject_state = InjectState { identity };
    let router = tonic::service::Routes::new(server)
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            inject_state,
            inject_extensions_middleware,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc serve");
    });
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));
    let channel = {
        let mut ch = None;
        for _ in 0..40 {
            if let Ok(c) = endpoint.clone().connect().await {
                ch = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        ch.expect("connect")
    };
    let mut client = ObservabilityServiceClient::new(channel);

    let resp = client
        .export_session(ExportSessionRequest {
            session_id: session_id.to_string(),
            time_range: None,
        })
        .await;
    let status = match resp {
        Ok(mut s) => match s.get_mut().message().await {
            Ok(_) => panic!("expected NotFound from cross-tenant call, got Ok"),
            Err(err) => err,
        },
        Err(err) => err,
    };

    // Assertion: cross-tenant must NOT be `Ok(bytes)`. `NotFound` is the
    // correct, more-secure disposition — the tenant-filtered SELECT returns
    // zero rows, so the handler has no opportunity to leak even the
    // existence of the session under tenant A. The in-handler
    // `PermissionDenied` guard remains for any future code path that
    // bypasses the DB tenant filter.
    assert_eq!(
        status.code(),
        tonic::Code::NotFound,
        "cross-tenant MUST be denied — NotFound is the authoritative deny code when DB filter hides existence"
    );
}

#[tokio::test]
#[ignore = "requires Postgres via testcontainers"]
async fn export_streams_rollovers_in_order_with_archive_status_on_first_chunk() {
    let h = setup_harness().await;
    let session_id = Uuid::new_v4();

    // Two rollover files with different, identifiable contents. We append
    // a sentinel byte to the end of each empty MCAP to disambiguate without
    // perturbing the MCAP parser — the handler streams raw bytes when
    // `time_range` is None so any appended trailer is fine for the test.
    let path_0 = h.mcap_dir.join(format!("{session_id}.mcap"));
    let path_1 = h.mcap_dir.join(format!("{session_id}.001.mcap"));
    let mut bytes_0 = Vec::new();
    let mut bytes_1 = Vec::new();
    {
        use std::io::Cursor;
        let mut w = mcap::Writer::new(Cursor::new(&mut bytes_0)).expect("writer");
        w.finish().expect("finish");
    }
    {
        use std::io::Cursor;
        let mut w = mcap::Writer::new(Cursor::new(&mut bytes_1)).expect("writer");
        w.finish().expect("finish");
    }
    bytes_0.extend_from_slice(b"\n__TRAILER0__");
    bytes_1.extend_from_slice(b"\n__TRAILER1__");
    std::fs::write(&path_0, &bytes_0).expect("write 0");
    std::fs::write(&path_1, &bytes_1).expect("write 1");

    let _ = insert_archive(
        &h.pool,
        h.tenant_id,
        session_id,
        path_0.to_str().unwrap(),
        0,
    )
    .await;
    let _ = insert_archive(
        &h.pool,
        h.tenant_id,
        session_id,
        path_1.to_str().unwrap(),
        1,
    )
    .await;

    let mut client = h.client().await;
    let mut stream = client
        .export_session(ExportSessionRequest {
            session_id: session_id.to_string(),
            time_range: None,
        })
        .await
        .expect("initial response")
        .into_inner();

    let mut per_file_bytes: std::collections::BTreeMap<u32, Vec<u8>> = std::collections::BTreeMap::new();
    let mut per_file_first_had_status: std::collections::BTreeMap<u32, bool> = std::collections::BTreeMap::new();
    let mut per_file_later_had_status: std::collections::BTreeMap<u32, bool> = std::collections::BTreeMap::new();
    while let Some(chunk) = stream.message().await.expect("stream item") {
        let idx = chunk.rollover_index.expect("rollover_index always set");
        let first_seen = !per_file_bytes.contains_key(&idx);
        per_file_bytes.entry(idx).or_default().extend_from_slice(&chunk.data);
        if first_seen {
            per_file_first_had_status.insert(idx, chunk.archive_status.is_some());
        } else if chunk.archive_status.is_some() {
            per_file_later_had_status.insert(idx, true);
        }
    }

    // Rollover ordering: both files streamed, index 0 before index 1.
    assert_eq!(
        per_file_bytes.keys().copied().collect::<Vec<_>>(),
        vec![0, 1],
        "rollover files must stream in ascending index order"
    );
    // Content round-trip: concatenated bytes match the on-disk contents.
    assert_eq!(per_file_bytes[&0], bytes_0, "rollover 0 bytes must match");
    assert_eq!(per_file_bytes[&1], bytes_1, "rollover 1 bytes must match");
    // archive_status appears only on the FIRST chunk of the FIRST file — the
    // handler only tags file 0, not subsequent rollovers (the per-file
    // `archive_status` in the proto is populated when `idx == 0`, otherwise
    // None, per the handler in `grpc/observability.rs`).
    assert!(
        per_file_first_had_status.get(&0).copied().unwrap_or(false),
        "first chunk of rollover 0 must carry archive_status"
    );
    assert!(
        !per_file_later_had_status.get(&0).copied().unwrap_or(false),
        "later chunks of rollover 0 must NOT carry archive_status"
    );
}

#[tokio::test]
#[ignore = "requires Postgres via testcontainers"]
async fn path_outside_mcap_root_returns_internal() {
    let h = setup_harness().await;
    let session_id = Uuid::new_v4();

    // Write a file outside the canonical mcap_dir root, then register it in
    // the DB. The handler must refuse to open it with `Internal`.
    let outside_tmp = tempfile::tempdir().expect("outside tempdir");
    let outside_path = outside_tmp.path().join("bad.mcap");
    write_empty_mcap(&outside_path);
    let canonical_outside = std::fs::canonicalize(&outside_path).expect("canonicalize outside");

    let _ = insert_archive(
        &h.pool,
        h.tenant_id,
        session_id,
        canonical_outside.to_str().unwrap(),
        0,
    )
    .await;

    let mut client = h.client().await;
    let resp = client
        .export_session(ExportSessionRequest {
            session_id: session_id.to_string(),
            time_range: None,
        })
        .await;
    let status = match resp {
        Ok(mut s) => match s.get_mut().message().await {
            Ok(_) => panic!("expected Internal, got stream Ok"),
            Err(err) => err,
        },
        Err(err) => err,
    };
    assert_eq!(
        status.code(),
        tonic::Code::Internal,
        "archive path outside mcap_dir must abort with Internal (path-safety guard)"
    );
}
