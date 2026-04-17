//! Phase 20 MCP-01/MCP-02/MCP-06: per-tenant CRUD for server-side MCP registrations.
//!
//! All functions are generic over `E: sqlx::Executor<'e, Database = sqlx::Postgres>`.
//! Tenant scoping is enforced by the RLS policies in
//! `migrations/20260415033_roz_mcp_servers.sql`; callers MUST invoke
//! `crate::set_tenant_context(&mut *tx, &tenant_id).await?` before queries.
//!
//! Auth secrets are never stored inline on `roz_mcp_servers`. Instead, rows
//! point at `roz_mcp_server_credentials(id)` through `credentials_ref`, and the
//! credential row carries AEAD ciphertext + nonce columns for bearer/header/
//! OAuth material.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct McpServerRow {
    pub tenant_id: Uuid,
    pub name: String,
    pub transport: String,
    pub url: String,
    pub credentials_ref: Option<Uuid>,
    pub enabled: bool,
    pub failure_count: i32,
    pub degraded_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct McpServerCredentialRow {
    pub tenant_id: Uuid,
    pub id: Uuid,
    pub auth_kind: String,
    pub header_name: Option<String>,
    pub bearer_ciphertext: Option<Vec<u8>>,
    pub bearer_nonce: Option<Vec<u8>>,
    pub header_value_ciphertext: Option<Vec<u8>>,
    pub header_value_nonce: Option<Vec<u8>>,
    pub oauth_access_ciphertext: Option<Vec<u8>>,
    pub oauth_access_nonce: Option<Vec<u8>>,
    pub oauth_refresh_ciphertext: Option<Vec<u8>>,
    pub oauth_refresh_nonce: Option<Vec<u8>>,
    pub oauth_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMcpServer {
    pub name: String,
    pub transport: String,
    pub url: String,
    pub credentials_ref: Option<Uuid>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct NewMcpServerCredential {
    pub id: Uuid,
    pub auth_kind: String,
    pub header_name: Option<String>,
    pub bearer_ciphertext: Option<Vec<u8>>,
    pub bearer_nonce: Option<Vec<u8>>,
    pub header_value_ciphertext: Option<Vec<u8>>,
    pub header_value_nonce: Option<Vec<u8>>,
    pub oauth_access_ciphertext: Option<Vec<u8>>,
    pub oauth_access_nonce: Option<Vec<u8>>,
    pub oauth_refresh_ciphertext: Option<Vec<u8>>,
    pub oauth_refresh_nonce: Option<Vec<u8>>,
    pub oauth_expires_at: Option<DateTime<Utc>>,
}

pub async fn upsert_credentials<'e, E>(executor: E, row: NewMcpServerCredential) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_mcp_server_credentials ( \
             tenant_id, id, auth_kind, header_name, \
             bearer_ciphertext, bearer_nonce, \
             header_value_ciphertext, header_value_nonce, \
             oauth_access_ciphertext, oauth_access_nonce, \
             oauth_refresh_ciphertext, oauth_refresh_nonce, oauth_expires_at \
         ) VALUES ( \
             current_setting('rls.tenant_id')::uuid, $1, $2, $3, \
             $4, $5, \
             $6, $7, \
             $8, $9, \
             $10, $11, $12 \
         ) \
         ON CONFLICT (tenant_id, id) DO UPDATE SET \
             auth_kind = EXCLUDED.auth_kind, \
             header_name = EXCLUDED.header_name, \
             bearer_ciphertext = EXCLUDED.bearer_ciphertext, \
             bearer_nonce = EXCLUDED.bearer_nonce, \
             header_value_ciphertext = EXCLUDED.header_value_ciphertext, \
             header_value_nonce = EXCLUDED.header_value_nonce, \
             oauth_access_ciphertext = EXCLUDED.oauth_access_ciphertext, \
             oauth_access_nonce = EXCLUDED.oauth_access_nonce, \
             oauth_refresh_ciphertext = EXCLUDED.oauth_refresh_ciphertext, \
             oauth_refresh_nonce = EXCLUDED.oauth_refresh_nonce, \
             oauth_expires_at = EXCLUDED.oauth_expires_at",
    )
    .bind(row.id)
    .bind(&row.auth_kind)
    .bind(&row.header_name)
    .bind(&row.bearer_ciphertext)
    .bind(&row.bearer_nonce)
    .bind(&row.header_value_ciphertext)
    .bind(&row.header_value_nonce)
    .bind(&row.oauth_access_ciphertext)
    .bind(&row.oauth_access_nonce)
    .bind(&row.oauth_refresh_ciphertext)
    .bind(&row.oauth_refresh_nonce)
    .bind(row.oauth_expires_at)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn get_credentials<'e, E>(executor: E, id: Uuid) -> Result<Option<McpServerCredentialRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerCredentialRow>(
        "SELECT tenant_id, id, auth_kind, header_name, \
                bearer_ciphertext, bearer_nonce, \
                header_value_ciphertext, header_value_nonce, \
                oauth_access_ciphertext, oauth_access_nonce, \
                oauth_refresh_ciphertext, oauth_refresh_nonce, oauth_expires_at, \
                created_at, updated_at \
         FROM roz_mcp_server_credentials \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await
}

pub async fn delete_credentials<'e, E>(executor: E, id: Uuid) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "DELETE FROM roz_mcp_server_credentials \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1",
    )
    .bind(id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

pub async fn upsert_server<'e, E>(executor: E, row: NewMcpServer) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_mcp_servers ( \
             tenant_id, name, transport, url, credentials_ref, enabled \
         ) VALUES ( \
             current_setting('rls.tenant_id')::uuid, $1, $2, $3, $4, $5 \
         ) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET \
             transport = EXCLUDED.transport, \
             url = EXCLUDED.url, \
             credentials_ref = EXCLUDED.credentials_ref, \
             enabled = EXCLUDED.enabled",
    )
    .bind(&row.name)
    .bind(&row.transport)
    .bind(&row.url)
    .bind(row.credentials_ref)
    .bind(row.enabled)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn get_server<'e, E>(executor: E, name: &str) -> Result<Option<McpServerRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerRow>(
        "SELECT tenant_id, name, transport, url, credentials_ref, enabled, \
                failure_count, degraded_at, last_error, created_at, updated_at \
         FROM roz_mcp_servers \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND name = $1",
    )
    .bind(name)
    .fetch_optional(executor)
    .await
}

pub async fn list_servers<'e, E>(executor: E) -> Result<Vec<McpServerRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerRow>(
        "SELECT tenant_id, name, transport, url, credentials_ref, enabled, \
                failure_count, degraded_at, last_error, created_at, updated_at \
         FROM roz_mcp_servers \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid \
         ORDER BY updated_at DESC",
    )
    .fetch_all(executor)
    .await
}

pub async fn list_enabled<'e, E>(executor: E) -> Result<Vec<McpServerRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerRow>(
        "SELECT tenant_id, name, transport, url, credentials_ref, enabled, \
                failure_count, degraded_at, last_error, created_at, updated_at \
         FROM roz_mcp_servers \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid \
           AND enabled = true \
         ORDER BY updated_at DESC",
    )
    .fetch_all(executor)
    .await
}

pub async fn delete_server<'e, E>(executor: E, name: &str) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "DELETE FROM roz_mcp_servers \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND name = $1",
    )
    .bind(name)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

pub async fn mark_degraded<'e, E>(
    executor: E,
    name: &str,
    last_error: &str,
) -> Result<Option<McpServerRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerRow>(
        "UPDATE roz_mcp_servers \
         SET failure_count = failure_count + 1, \
             degraded_at = COALESCE(degraded_at, now()), \
             last_error = $2 \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND name = $1 \
         RETURNING tenant_id, name, transport, url, credentials_ref, enabled, \
                   failure_count, degraded_at, last_error, created_at, updated_at",
    )
    .bind(name)
    .bind(last_error)
    .fetch_optional(executor)
    .await
}

pub async fn clear_degraded<'e, E>(executor: E, name: &str) -> Result<Option<McpServerRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, McpServerRow>(
        "UPDATE roz_mcp_servers \
         SET failure_count = 0, degraded_at = NULL, last_error = NULL \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND name = $1 \
         RETURNING tenant_id, name, transport, url, credentials_ref, enabled, \
                   failure_count, degraded_at, last_error, created_at, updated_at",
    )
    .bind(name)
    .fetch_optional(executor)
    .await
}
