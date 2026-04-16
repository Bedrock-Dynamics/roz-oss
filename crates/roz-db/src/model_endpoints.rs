//! Phase 19 OWM-06: per-tenant CRUD for `roz_model_endpoints`.
//!
//! **Cloud-facing.** This module ships in Phase 19 for future cloud consumers
//! that will wrap or replace `roz_core::EndpointRegistry` with a DB-backed
//! variant. The OSS `EndpointRegistry` in `roz-core` does NOT consume this
//! layer this phase — OSS loads endpoints from `endpoints.toml` at boot.
//!
//! All functions are generic over `E: sqlx::Executor<'e, Database = sqlx::Postgres>`
//! per CLAUDE.md DB conventions. Tenant scoping is enforced by the RLS policy
//! `tenant_isolation` defined in `migrations/20260415032_model_endpoints.sql`;
//! callers MUST invoke `crate::set_tenant_context(&mut *tx, &tenant_id).await?`
//! inside a transaction before any query.
//!
//! Credential bytes (`api_key_ciphertext`, `api_key_nonce`,
//! `oauth_token_ciphertext`, `oauth_token_nonce`, `oauth_refresh_ciphertext`,
//! `oauth_refresh_nonce`) are AEAD ciphertext managed by callers via
//! `roz_core::KeyProvider` (Plan 19-03). The CHECK constraint
//! `roz_model_endpoints_auth_shape` enforces the shape invariant — see
//! `T-19-04-02` mitigation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Full row from `roz_model_endpoints`.
///
/// Matches the DDL in `migrations/20260415032_model_endpoints.sql`. Composite
/// PK: `(tenant_id, name)`.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ModelEndpointRow {
    pub tenant_id: Uuid,
    pub name: String,
    pub base_url: String,
    pub auth_mode: String,
    pub wire_api: String,
    pub tool_call_format: Option<String>,
    pub reasoning_format: Option<String>,
    pub api_key_ciphertext: Option<Vec<u8>>,
    pub api_key_nonce: Option<Vec<u8>>,
    pub oauth_token_ciphertext: Option<Vec<u8>>,
    pub oauth_token_nonce: Option<Vec<u8>>,
    pub oauth_refresh_ciphertext: Option<Vec<u8>>,
    pub oauth_refresh_nonce: Option<Vec<u8>>,
    pub oauth_expires_at: Option<DateTime<Utc>>,
    pub oauth_account_id: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insertable shape for [`upsert`].
///
/// `tenant_id` is read from `current_setting('rls.tenant_id')::uuid` at the DB
/// layer (defense-in-depth: a connection without RLS context will fail to
/// insert). The `created_at` / `updated_at` columns default and update via
/// trigger.
#[derive(Debug, Clone)]
pub struct NewModelEndpoint {
    pub name: String,
    pub base_url: String,
    pub auth_mode: String,
    pub wire_api: String,
    pub tool_call_format: Option<String>,
    pub reasoning_format: Option<String>,
    pub api_key_ciphertext: Option<Vec<u8>>,
    pub api_key_nonce: Option<Vec<u8>>,
    pub oauth_token_ciphertext: Option<Vec<u8>>,
    pub oauth_token_nonce: Option<Vec<u8>>,
    pub oauth_refresh_ciphertext: Option<Vec<u8>>,
    pub oauth_refresh_nonce: Option<Vec<u8>>,
    pub oauth_expires_at: Option<DateTime<Utc>>,
    pub oauth_account_id: Option<String>,
    pub enabled: bool,
}

/// Upsert by composite PK `(tenant_id, name)`. Tenant comes from RLS context.
///
/// On conflict, all credential and routing columns are replaced; the trigger
/// `roz_model_endpoints_touch_trg` refreshes `updated_at`.
///
/// # Errors
///
/// Returns `sqlx::Error::Database` on `roz_model_endpoints_auth_shape` CHECK
/// violation (e.g. `auth_mode='api_key'` without `api_key_ciphertext` /
/// `api_key_nonce`), RLS rejection, or unknown enum-string for `auth_mode`
/// or `wire_api`.
pub async fn upsert<'e, E>(executor: E, row: NewModelEndpoint) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_model_endpoints ( \
             tenant_id, name, base_url, auth_mode, wire_api, \
             tool_call_format, reasoning_format, \
             api_key_ciphertext, api_key_nonce, \
             oauth_token_ciphertext, oauth_token_nonce, \
             oauth_refresh_ciphertext, oauth_refresh_nonce, \
             oauth_expires_at, oauth_account_id, enabled \
         ) VALUES ( \
             current_setting('rls.tenant_id')::uuid, $1, $2, $3, $4, \
             $5, $6, \
             $7, $8, \
             $9, $10, \
             $11, $12, \
             $13, $14, $15 \
         ) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET \
             base_url = EXCLUDED.base_url, \
             auth_mode = EXCLUDED.auth_mode, \
             wire_api = EXCLUDED.wire_api, \
             tool_call_format = EXCLUDED.tool_call_format, \
             reasoning_format = EXCLUDED.reasoning_format, \
             api_key_ciphertext = EXCLUDED.api_key_ciphertext, \
             api_key_nonce = EXCLUDED.api_key_nonce, \
             oauth_token_ciphertext = EXCLUDED.oauth_token_ciphertext, \
             oauth_token_nonce = EXCLUDED.oauth_token_nonce, \
             oauth_refresh_ciphertext = EXCLUDED.oauth_refresh_ciphertext, \
             oauth_refresh_nonce = EXCLUDED.oauth_refresh_nonce, \
             oauth_expires_at = EXCLUDED.oauth_expires_at, \
             oauth_account_id = EXCLUDED.oauth_account_id, \
             enabled = EXCLUDED.enabled",
    )
    .bind(&row.name)
    .bind(&row.base_url)
    .bind(&row.auth_mode)
    .bind(&row.wire_api)
    .bind(&row.tool_call_format)
    .bind(&row.reasoning_format)
    .bind(&row.api_key_ciphertext)
    .bind(&row.api_key_nonce)
    .bind(&row.oauth_token_ciphertext)
    .bind(&row.oauth_token_nonce)
    .bind(&row.oauth_refresh_ciphertext)
    .bind(&row.oauth_refresh_nonce)
    .bind(row.oauth_expires_at)
    .bind(&row.oauth_account_id)
    .bind(row.enabled)
    .execute(executor)
    .await?;
    Ok(())
}

/// Fetch one row by `name` inside the current tenant (RLS). Returns `None`
/// when absent or hidden by RLS.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS-context misconfiguration or connection
/// failure.
pub async fn get<'e, E>(executor: E, name: &str) -> Result<Option<ModelEndpointRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ModelEndpointRow>(
        "SELECT tenant_id, name, base_url, auth_mode, wire_api, \
                tool_call_format, reasoning_format, \
                api_key_ciphertext, api_key_nonce, \
                oauth_token_ciphertext, oauth_token_nonce, \
                oauth_refresh_ciphertext, oauth_refresh_nonce, \
                oauth_expires_at, oauth_account_id, \
                enabled, created_at, updated_at \
         FROM roz_model_endpoints \
         WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(executor)
    .await
}

/// List all enabled endpoints for the current tenant, ordered by
/// `updated_at DESC` (matches the partial index
/// `roz_model_endpoints_enabled_lookup`).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS-context misconfiguration or connection
/// failure.
pub async fn list_enabled<'e, E>(executor: E) -> Result<Vec<ModelEndpointRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ModelEndpointRow>(
        "SELECT tenant_id, name, base_url, auth_mode, wire_api, \
                tool_call_format, reasoning_format, \
                api_key_ciphertext, api_key_nonce, \
                oauth_token_ciphertext, oauth_token_nonce, \
                oauth_refresh_ciphertext, oauth_refresh_nonce, \
                oauth_expires_at, oauth_account_id, \
                enabled, created_at, updated_at \
         FROM roz_model_endpoints \
         WHERE enabled = true \
         ORDER BY updated_at DESC",
    )
    .fetch_all(executor)
    .await
}

/// Delete one endpoint by `name` inside the current tenant. Returns the
/// number of rows removed (0 or 1).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS-context misconfiguration or connection
/// failure.
pub async fn delete<'e, E>(executor: E, name: &str) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_model_endpoints WHERE name = $1")
        .bind(name)
        .execute(executor)
        .await?;
    Ok(result.rows_affected())
}
