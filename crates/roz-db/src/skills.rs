//! Phase 18 SKILL-01: per-tenant skill library CRUD over the composite-PK `roz_skills` table.
//!
//! Replaces the Phase 4 surrogate-key shape (verified to have no production
//! write path — see RESEARCH §Runtime State Inventory).
//!
//! All functions are generic over `E: sqlx::Executor<'e, Database = sqlx::Postgres>`
//! per CLAUDE.md DB conventions. Tenant scoping is enforced by the RLS policy
//! `tenant_isolation` defined in migration 031; callers MUST invoke
//! `crate::set_tenant_context(&mut *tx, &tenant_id).await?` before any query.

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Full row from `roz_skills` (composite PK: `(tenant_id, name, version)`).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SkillRow {
    pub tenant_id: Uuid,
    pub name: String,
    pub version: String,
    pub body_md: String,
    pub frontmatter: serde_json::Value,
    pub source: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Tier-0 PromptAssembler projection — small enough for the N=20 @ ≤3 KB block
/// budget defined in CONTEXT D-12.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub version: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

/// SKILL-01: insert a new skill version.
///
/// Caller MUST have invoked `crate::set_tenant_context(&mut *tx, &tenant_id)`.
/// Composite-PK collision returns `sqlx::Error::Database` with constraint
/// `roz_skills_pkey`; the gRPC layer (PLAN-05) maps to `Status::already_exists`
/// (D-06 — npm/crates.io immutability).
///
/// Defense-in-depth: `tenant_id` is read from `current_setting('rls.tenant_id')`
/// (NOT from a caller param) so an RLS-disabled connection refuses to misroute
/// cross-tenant (T-18-03-01 mitigation).
///
/// # Errors
///
/// Returns `sqlx::Error` on PK collision, CHECK-constraint violation
/// (`name` regex, `description` length, `name` length) or RLS rejection.
pub async fn insert_skill<'e, E>(
    executor: E,
    name: &str,
    version: &str,
    body_md: &str,
    frontmatter: &serde_json::Value,
    source: &str,
    created_by: &str,
) -> Result<SkillRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SkillRow>(
        "INSERT INTO roz_skills \
             (tenant_id, name, version, body_md, frontmatter, source, created_by) \
         VALUES (current_setting('rls.tenant_id')::uuid, $1, $2, $3, $4, $5, $6) \
         RETURNING tenant_id, name, version, body_md, frontmatter, source, \
                   created_by, created_at, updated_at",
    )
    .bind(name)
    .bind(version)
    .bind(body_md)
    .bind(frontmatter)
    .bind(source)
    .bind(created_by)
    .fetch_one(executor)
    .await
}

/// D-12 / SKILL-05: most-recent N skills for the tier-0 PromptAssembler block.
///
/// Returns rows ordered by `created_at DESC`. Tenant scoping is enforced by RLS
/// (caller invokes `set_tenant_context` first). `description` is extracted from
/// `frontmatter->>'description'` and defaults to `""` if absent (CHECK on the
/// table forbids empty descriptions, so `""` only surfaces for rows that
/// pre-existed without a description — defensive).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn list_recent<'e, E>(executor: E, limit: i64) -> Result<Vec<SkillSummary>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SkillSummary>(
        "SELECT name, version, \
                COALESCE(frontmatter->>'description', '') AS description, \
                created_at, created_by \
         FROM roz_skills \
         ORDER BY created_at DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(executor)
    .await
}

/// Fetch one row by the full composite key component `(name, version)` inside
/// the current tenant (RLS). Returns `None` when absent.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn get_by_name_version<'e, E>(executor: E, name: &str, version: &str) -> Result<Option<SkillRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SkillRow>(
        "SELECT tenant_id, name, version, body_md, frontmatter, source, \
                created_by, created_at, updated_at \
         FROM roz_skills \
         WHERE name = $1 AND version = $2",
    )
    .bind(name)
    .bind(version)
    .fetch_optional(executor)
    .await
}

/// RESEARCH OQ #2: client-side semver sort over up to 50 candidate rows.
///
/// We fetch the 50 most-recent versions of `name` and pick the one with the
/// maximum `semver::Version`. Rows with unparseable `version` strings sort
/// below valid ones (via the `max_by` ordering tiebreak below). Returns `None`
/// if no rows match `name` inside the current tenant (RLS).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn get_latest_by_semver<'e, E>(executor: E, name: &str) -> Result<Option<SkillRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query_as::<_, SkillRow>(
        "SELECT tenant_id, name, version, body_md, frontmatter, source, \
                created_by, created_at, updated_at \
         FROM roz_skills \
         WHERE name = $1 \
         ORDER BY created_at DESC \
         LIMIT 50",
    )
    .bind(name)
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .max_by(|a, b| match (Version::parse(&a.version), Version::parse(&b.version)) {
            (Ok(va), Ok(vb)) => va.cmp(&vb),
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Err(_), Err(_)) => std::cmp::Ordering::Equal,
        }))
}

/// SKILL-07 + D-15: CLI-only delete. `version: None` deletes every version of
/// `name` inside the current tenant (RLS); `version: Some(v)` deletes exactly
/// one row. Returns the number of rows removed.
///
/// gRPC layer (PLAN-05) gates `Delete` on `Permissions::can_write_skills`; this
/// DB helper is not exposed to the agent (T-18-03-05 mitigation).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn delete_skill<'e, E>(executor: E, name: &str, version: Option<&str>) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = match version {
        Some(v) => {
            sqlx::query("DELETE FROM roz_skills WHERE name = $1 AND version = $2")
                .bind(name)
                .bind(v)
                .execute(executor)
                .await?
        }
        None => {
            sqlx::query("DELETE FROM roz_skills WHERE name = $1")
                .bind(name)
                .execute(executor)
                .await?
        }
    };
    Ok(result.rows_affected())
}

/// Pure helper isolated for unit-test (no DB needed). Client-side semver sort
/// mirroring [`get_latest_by_semver`]'s max-by predicate so the sort invariant
/// is test-covered without a live database.
#[must_use]
pub fn pick_latest_semver(versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok().map(|p| (p, v.clone())))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, raw)| raw)
}

#[cfg(test)]
mod tests {
    use super::pick_latest_semver;

    #[test]
    fn latest_by_semver_picks_max_prerelease_aware() {
        let versions: Vec<String> = ["1.2.3", "1.2.3-dev.1", "1.2.3-dev.10", "1.2.4-rc.1", "1.2.2"]
            .into_iter()
            .map(String::from)
            .collect();
        // Semver precedence: 1.2.4-rc.1 wins because (1,2,4) > (1,2,3).
        // Proves the helper inherits semver::Version ordering.
        assert_eq!(pick_latest_semver(&versions).as_deref(), Some("1.2.4-rc.1"));
    }

    #[test]
    fn latest_by_semver_handles_empty() {
        let empty: Vec<String> = Vec::new();
        assert!(pick_latest_semver(&empty).is_none());
    }

    #[test]
    fn latest_by_semver_skips_unparseable() {
        let versions: Vec<String> = vec!["not-a-version".into(), "1.0.0".into()];
        assert_eq!(pick_latest_semver(&versions).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn latest_by_semver_numeric_prerelease_ordering() {
        // Proves we use semver::Version::cmp (numeric prerelease) not lexical —
        // lexical ordering would return "dev.1" as larger than "dev.10".
        let versions: Vec<String> = vec!["1.2.3-dev.1".into(), "1.2.3-dev.10".into()];
        assert_eq!(pick_latest_semver(&versions).as_deref(), Some("1.2.3-dev.10"));
    }
}
