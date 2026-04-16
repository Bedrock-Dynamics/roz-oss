//! Integration tests that exercise the REAL gRPC auth middleware stack —
//! provision real API keys in Postgres, hit the real `ApiKeyAuth`
//! validator, and verify that `Permissions` derived by
//! `roz_server::auth::permissions_for_identity` flow all the way through
//! to gated RPCs on `SkillsServiceImpl`.
//!
//! Unlike `skills_grpc_integration.rs`, this suite does NOT use the
//! bypass `inject_extensions_middleware`. It wires the production
//! `grpc_auth_middleware` so that any regression in the
//! `AuthIdentity` → `Permissions` derivation chain is caught here.
//!
//! Closes the Phase 18 UAT Test 11 gap (`roz skill delete` rejected with
//! `Delete requires can_write_skills` despite an admin-scoped API key).
//!
//! ```bash
//! cargo test -p roz-server --test skills_grpc_real_auth_integration \
//!     -- --ignored --test-threads=1
//! ```

#![allow(clippy::too_many_lines, reason = "integration tests have unavoidable scaffolding")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use object_store::ObjectStore;
use roz_db::{create_pool, run_migrations};
use roz_server::auth::ApiKeyAuth;
use roz_server::grpc::roz_v1::skills_service_client::SkillsServiceClient;
use roz_server::grpc::roz_v1::{DeleteSkillRequest, ImportChunk, ImportHeader, import_chunk};
use roz_server::grpc::skills::SkillsServiceImpl;
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};
use sqlx::PgPool;
use tempfile::TempDir;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const VALID_SKILL_MD: &str = "---\n\
name: real-auth-skill\n\
description: Phase 18-12 real-auth fixture\n\
version: 0.1.0\n\
---\n\
# Real Auth Skill\n\nbody\n";

const VALID_HELLO: &[u8] = b"#!/usr/bin/env bash\necho real-auth\n";

fn build_valid_tar_gz() -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut h = tar::Header::new_gnu();
        h.set_size(VALID_SKILL_MD.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder
            .append_data(&mut h, "SKILL.md", VALID_SKILL_MD.as_bytes())
            .expect("append SKILL.md");
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(VALID_HELLO.len() as u64);
        h2.set_mode(0o755);
        h2.set_cksum();
        builder
            .append_data(&mut h2, "scripts/hello.sh", VALID_HELLO)
            .expect("append hello.sh");
        builder.finish().expect("tar finish");
    }
    encoder.finish().expect("gz finish")
}

fn import_stream(bytes: &[u8], chunk_size: usize) -> Vec<ImportChunk> {
    let mut chunks = vec![ImportChunk {
        chunk: Some(import_chunk::Chunk::Header(ImportHeader {
            source: "./real-auth-fixture/".into(),
            total_size_bytes: bytes.len() as u64,
        })),
    }];
    for c in bytes.chunks(chunk_size.max(1)) {
        chunks.push(ImportChunk {
            chunk: Some(import_chunk::Chunk::TarGzBytes(c.to_vec())),
        });
    }
    chunks
}

// ---------------------------------------------------------------------------
// Harness: real ApiKeyAuth + grpc_auth_middleware + SkillsServiceImpl.
// ---------------------------------------------------------------------------

struct Harness {
    addr: SocketAddr,
    pool: PgPool,
    tenant_id: Uuid,
    admin_key: String,
    readonly_key: String,
    _tmp: TempDir,
}

async fn setup_harness() -> Harness {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    // Keep the container alive for the whole process.
    std::mem::forget(guard);
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    let slug = format!("skills-real-auth-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Skills Real Auth", &slug, "personal")
        .await
        .expect("tenant");

    // Admin-scoped key mirrors the exact string main.rs writes.
    let admin = roz_db::api_keys::create_api_key(&pool, tenant.id, "Admin Key", &["admin".into()], "test")
        .await
        .expect("admin api key");

    // Non-admin key: scope that exists in `ApiKeyScope` but is not `Admin`.
    let readonly = roz_db::api_keys::create_api_key(
        &pool,
        tenant.id,
        "Read-Only Key",
        &["read-tasks".into(), "read-streams".into()],
        "test",
    )
    .await
    .expect("readonly api key");

    let tmp = tempfile::tempdir().expect("tempdir");
    let object_store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(tmp.path()).expect("LocalFileSystem"));

    let svc = SkillsServiceImpl::new(pool.clone(), object_store);

    let grpc_auth_state = GrpcAuthState {
        auth: Arc::new(ApiKeyAuth),
        pool: pool.clone(),
    };

    let router = tonic::service::Routes::new(svc.into_server())
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            grpc_auth_state,
            grpc_auth_middleware,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    Harness {
        addr,
        pool,
        tenant_id: tenant.id,
        admin_key: admin.full_key,
        readonly_key: readonly.full_key,
        _tmp: tmp,
    }
}

/// Connect a `SkillsServiceClient` whose interceptor attaches
/// `authorization: Bearer <token>` on every RPC — mirrors the real
/// roz-cli channel setup (see `crates/roz-cli/src/commands/skill.rs`).
type BearerInterceptor = Box<dyn FnMut(Request<()>) -> Result<Request<()>, tonic::Status> + Send>;

async fn connect_with_bearer(
    addr: SocketAddr,
    bearer: String,
) -> SkillsServiceClient<InterceptedService<Channel, BearerInterceptor>> {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));

    let mut last: Option<tonic::transport::Error> = None;
    let channel = loop {
        match endpoint.clone().connect().await {
            Ok(c) => break c,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };
    drop(last);

    let auth_value: MetadataValue<_> = format!("Bearer {bearer}").parse().expect("valid metadata");
    let interceptor: BearerInterceptor = Box::new(move |mut req: Request<()>| -> Result<Request<()>, tonic::Status> {
        req.metadata_mut().insert("authorization", auth_value.clone());
        Ok(req)
    });

    SkillsServiceClient::with_interceptor(channel, interceptor)
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Admin-scoped API key MUST reach `SkillsServiceImpl::delete` and see
/// `can_write_skills = true`. Before 18-12 this failed with
/// `PermissionDenied` because the middleware never attached a
/// `Permissions` extension.
#[tokio::test]
#[ignore = "requires docker"]
async fn admin_scoped_key_can_delete_skill_through_real_auth_stack() {
    let h = setup_harness().await;
    let mut client = connect_with_bearer(h.addr, h.admin_key.clone()).await;

    // Import succeeds with admin key.
    let bytes = build_valid_tar_gz();
    let stream = import_stream(&bytes, 64 * 1024);
    let resp = client
        .import(Request::new(tokio_stream::iter(stream)))
        .await
        .expect("import OK")
        .into_inner();
    let meta = resp.meta.expect("meta");
    assert_eq!(meta.name, "real-auth-skill");
    assert_eq!(meta.version, "0.1.0");

    // Delete succeeds: exercises the full auth → Permissions → skills.delete chain.
    let del = client
        .delete(Request::new(DeleteSkillRequest {
            name: "real-auth-skill".into(),
            version: Some("0.1.0".into()),
        }))
        .await
        .expect("delete OK")
        .into_inner();
    assert_eq!(
        del.versions_deleted, 1,
        "admin-scoped delete through real auth stack must remove exactly one row"
    );

    // The backing DB row is gone.
    let mut tx = h.pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &h.tenant_id).await.unwrap();
    let remaining = roz_db::skills::get_by_name_version(&mut *tx, "real-auth-skill", "0.1.0")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(remaining.is_none(), "row must be deleted after admin delete");
}

/// Non-admin key (scope != `Admin`) MUST be rejected by the real
/// permission gate with `PermissionDenied`, not bypassed by the
/// test middleware. Regression fence for 18-12.
#[tokio::test]
#[ignore = "requires docker"]
async fn non_admin_key_cannot_delete_skill_through_real_auth_stack() {
    let h = setup_harness().await;

    // Seed the library with one skill using the admin key so there IS
    // something to try (and fail) to delete.
    {
        let mut admin_client = connect_with_bearer(h.addr, h.admin_key.clone()).await;
        let bytes = build_valid_tar_gz();
        let stream = import_stream(&bytes, 64 * 1024);
        admin_client
            .import(Request::new(tokio_stream::iter(stream)))
            .await
            .expect("admin import");
    }

    // Switch to the read-only key.
    let mut readonly_client = connect_with_bearer(h.addr, h.readonly_key.clone()).await;

    let err = readonly_client
        .delete(Request::new(DeleteSkillRequest {
            name: "real-auth-skill".into(),
            version: Some("0.1.0".into()),
        }))
        .await
        .expect_err("non-admin must be denied");

    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "expected PermissionDenied through real middleware, got {err:?}"
    );

    // Row must still exist — the permission gate fired before any DB work.
    let mut tx = h.pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &h.tenant_id).await.unwrap();
    let row = roz_db::skills::get_by_name_version(&mut *tx, "real-auth-skill", "0.1.0")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(row.is_some(), "denied delete must not have mutated the DB");
}

/// Missing auth header MUST be rejected at the middleware layer with
/// `UNAUTHENTICATED`, never reaching the service.
#[tokio::test]
#[ignore = "requires docker"]
async fn missing_auth_header_returns_unauthenticated() {
    let h = setup_harness().await;

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{}", h.addr))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));
    let channel = endpoint.connect().await.expect("connect");
    let mut client = SkillsServiceClient::new(channel);

    let err = client
        .delete(Request::new(DeleteSkillRequest {
            name: "real-auth-skill".into(),
            version: Some("0.1.0".into()),
        }))
        .await
        .expect_err("no auth header must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "expected Unauthenticated, got {err:?}"
    );
}
