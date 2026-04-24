//! Phase 26.7 D-02: CRUD helpers for `roz_session_artifacts`.
//!
//! Mirrors `crates/roz-db/src/mcap_archives.rs` — the structural template.
//!
//! # Tenant scoping
//! The server pool runs as a Postgres superuser which bypasses RLS (see
//! `crates/roz-server/src/grpc/observability.rs:115-135` comment). Queries
//! therefore bind `tenant_id` EXPLICITLY in SQL — NOT via
//! `set_tenant_context`. Callers (handler code) pass the caller's tenant
//! UUID derived from `AuthIdentity` at the RPC boundary. Defense-in-depth
//! is provided by a handler-side `row.tenant_id != caller_tenant` check.
//! Retention helpers (`list_retention_candidates`,
//! `list_finalized_ordered_desc`) run without tenant scope — the sweeper
//! is server-wide.

use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

/// Row type matching `roz_session_artifacts` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SessionArtifactRow {
    pub artifact_id: Uuid,
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub artifact_type: String,
    pub path: String,
    pub digest_sha256: Vec<u8>,
    pub size_bytes: i64,
    pub content_type: String,
    pub uploaded_at: DateTime<Utc>,
}

/// Insert a new artifact row.
///
/// # Errors
/// Returns `sqlx::Error::Database` with Postgres unique-violation code
/// `23505` if the `(session_id, artifact_type, path)` tuple already exists.
#[expect(
    clippy::too_many_arguments,
    reason = "column-per-parameter mirrors the roz_session_artifacts schema; grouping into a struct would add boilerplate without improving call sites"
)]
pub async fn insert<'e, E>(
    executor: E,
    tenant_id: Uuid,
    session_id: Uuid,
    artifact_type: &str,
    path: &str,
    digest_sha256: &[u8],
    size_bytes: i64,
    content_type: &str,
) -> Result<SessionArtifactRow, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionArtifactRow>(
        "INSERT INTO roz_session_artifacts \
         (tenant_id, session_id, artifact_type, path, digest_sha256, size_bytes, content_type) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(artifact_type)
    .bind(path)
    .bind(digest_sha256)
    .bind(size_bytes)
    .bind(content_type)
    .fetch_one(executor)
    .await
}

/// List all artifacts for a (tenant, session). tenant_id is bound
/// explicitly in SQL (superuser pool bypasses RLS).
pub async fn list_by_session<'e, E>(
    executor: E,
    tenant_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<SessionArtifactRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionArtifactRow>(
        "SELECT * FROM roz_session_artifacts \
         WHERE tenant_id = $1 AND session_id = $2 \
         ORDER BY uploaded_at ASC, artifact_type ASC, path ASC",
    )
    .bind(tenant_id)
    .bind(session_id)
    .fetch_all(executor)
    .await
}

/// Fetch a single artifact by id + tenant.
///
/// Returns `None` if either the row does not exist or the tenant does
/// not match — cross-tenant access looks identical to not-found.
pub async fn fetch_by_id<'e, E>(
    executor: E,
    tenant_id: Uuid,
    artifact_id: Uuid,
) -> Result<Option<SessionArtifactRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionArtifactRow>(
        "SELECT * FROM roz_session_artifacts \
         WHERE tenant_id = $1 AND artifact_id = $2",
    )
    .bind(tenant_id)
    .bind(artifact_id)
    .fetch_optional(executor)
    .await
}

/// Retention TTL pass — server-wide, no tenant scope.
/// Callers: `crates/roz-server/src/observability/retention.rs`.
pub async fn list_retention_candidates<'e, E>(
    executor: E,
    ttl_secs: i64,
) -> Result<Vec<SessionArtifactRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionArtifactRow>(
        "SELECT * FROM roz_session_artifacts \
         WHERE uploaded_at < (now() - ($1 || ' seconds')::interval) \
         ORDER BY uploaded_at ASC",
    )
    .bind(ttl_secs.to_string())
    .fetch_all(executor)
    .await
}

/// Retention size-cap pass — server-wide, newest-first for accumulation.
pub async fn list_finalized_ordered_desc<'e, E>(executor: E) -> Result<Vec<SessionArtifactRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionArtifactRow>("SELECT * FROM roz_session_artifacts ORDER BY uploaded_at DESC")
        .fetch_all(executor)
        .await
}

/// Delete a row by id. Returns rows affected. Called by the retention
/// sweeper (server-wide scope); no tenant bind needed.
pub async fn delete_by_id(pool: &PgPool, artifact_id: Uuid) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM roz_session_artifacts WHERE artifact_id = $1")
        .bind(artifact_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    // Integration tests that need a real Postgres container live in
    // crates/roz-db/tests or gated behind `test-helpers`. Keep unit
    // tests focused on pure logic only — SessionArtifactRow is pure data.
    use super::SessionArtifactRow;

    #[test]
    fn row_struct_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionArtifactRow>();
    }
}
