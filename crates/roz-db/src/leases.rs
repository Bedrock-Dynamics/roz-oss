use sqlx::PgPool;
use uuid::Uuid;

/// Row type matching the `roz_capability_leases` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct LeaseRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub resource: String,
    pub holder_id: String,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub released_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Acquire a new capability lease with a TTL in seconds.
/// Sets `expires_at = now() + make_interval(secs => ttl_secs)`.
pub async fn acquire(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    resource: &str,
    holder_id: &str,
    ttl_secs: i64,
) -> Result<LeaseRow, sqlx::Error> {
    #[allow(clippy::cast_precision_loss)] // TTL values are always small enough for f64
    let ttl = ttl_secs as f64;
    sqlx::query_as::<_, LeaseRow>(
        "INSERT INTO roz_capability_leases (tenant_id, host_id, resource, holder_id, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5)) RETURNING *",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(resource)
    .bind(holder_id)
    .bind(ttl)
    .fetch_one(pool)
    .await
}

/// Fetch a single lease by primary key, or `None` if not found.
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<LeaseRow>, sqlx::Error> {
    sqlx::query_as::<_, LeaseRow>("SELECT * FROM roz_capability_leases WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Release a lease by setting `released_at = now()`.
/// Only releases if not already released. Returns `None` when the row does not exist
/// or is already released.
pub async fn release(pool: &PgPool, id: Uuid) -> Result<Option<LeaseRow>, sqlx::Error> {
    sqlx::query_as::<_, LeaseRow>(
        "UPDATE roz_capability_leases \
         SET released_at = now() \
         WHERE id = $1 AND released_at IS NULL \
         RETURNING *",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// List active leases for a tenant (not released and not yet expired).
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list_active(pool: &PgPool, tenant_id: Uuid) -> Result<Vec<LeaseRow>, sqlx::Error> {
    sqlx::query_as::<_, LeaseRow>(
        "SELECT * FROM roz_capability_leases \
         WHERE tenant_id = $1 AND released_at IS NULL AND expires_at > now() \
         ORDER BY acquired_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

/// Expire stale leases: set `released_at = now()` for all leases where
/// `released_at IS NULL AND expires_at <= now()`. Returns the number of rows affected.
pub async fn expire_stale(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE roz_capability_leases \
         SET released_at = now() \
         WHERE released_at IS NULL AND expires_at <= now()",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> PgPool {
        crate::shared_test_pool().await
    }

    async fn create_test_tenant(pool: &PgPool) -> Uuid {
        let slug = format!("test-{}", Uuid::new_v4());
        crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant")
            .id
    }

    async fn create_test_host(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::hosts::create(pool, tenant_id, "test-host", "edge", &[], &serde_json::json!({}))
            .await
            .expect("Failed to create host")
            .id
    }

    #[tokio::test]
    async fn acquire_and_get_lease() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let lease = acquire(&pool, tenant_id, host_id, "arm_gripper", "worker-1", 300)
            .await
            .expect("Failed to acquire lease");

        assert_eq!(lease.tenant_id, tenant_id);
        assert_eq!(lease.host_id, host_id);
        assert_eq!(lease.resource, "arm_gripper");
        assert_eq!(lease.holder_id, "worker-1");
        assert!(lease.released_at.is_none());
        assert!(lease.expires_at > lease.acquired_at);

        let fetched = get_by_id(&pool, lease.id)
            .await
            .expect("Failed to get lease")
            .expect("Lease should exist");

        assert_eq!(fetched.id, lease.id);
        assert_eq!(fetched.resource, "arm_gripper");
    }

    #[tokio::test]
    async fn release_lease() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let lease = acquire(&pool, tenant_id, host_id, "camera", "worker-2", 600)
            .await
            .expect("Failed to acquire lease");

        assert!(lease.released_at.is_none());

        let released = release(&pool, lease.id)
            .await
            .expect("Failed to release lease")
            .expect("Lease should exist and be unreleased");

        assert!(released.released_at.is_some());

        // Releasing again returns None (already released)
        let again = release(&pool, lease.id).await.expect("Failed to release again");
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn list_active_leases() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        // Create an active lease (long TTL)
        let active = acquire(&pool, tenant_id, host_id, "sensor-1", "w1", 3600)
            .await
            .expect("Failed to acquire active lease");

        // Create a lease and release it
        let released_lease = acquire(&pool, tenant_id, host_id, "sensor-2", "w2", 3600)
            .await
            .expect("Failed to acquire lease to release");
        release(&pool, released_lease.id)
            .await
            .expect("Failed to release lease");

        let active_leases = list_active(&pool, tenant_id).await.expect("Failed to list active");
        assert!(
            active_leases.iter().any(|l| l.id == active.id),
            "active lease should appear in list_active"
        );
        assert!(
            !active_leases.iter().any(|l| l.id == released_lease.id),
            "released lease should not appear in list_active"
        );
    }

    #[tokio::test]
    async fn expire_stale_leases() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        // Insert a lease that expired in the past via raw SQL
        let stale_id: (Uuid,) = sqlx::query_as(
            "INSERT INTO roz_capability_leases (tenant_id, host_id, resource, holder_id, expires_at) \
             VALUES ($1, $2, 'stale-resource', 'w-stale', now() - interval '1 hour') RETURNING id",
        )
        .bind(tenant_id)
        .bind(host_id)
        .fetch_one(&pool)
        .await
        .expect("Failed to insert stale lease");

        let count = expire_stale(&pool).await.expect("Failed to expire stale");
        assert!(count >= 1, "expected at least 1 stale lease expired, got {count}");

        // Verify the stale lease is now released
        let lease = get_by_id(&pool, stale_id.0)
            .await
            .expect("Failed to get stale lease")
            .expect("Lease should exist");
        assert!(lease.released_at.is_some());
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let host_a = create_test_host(&pool, tenant_a).await;
        let host_b = create_test_host(&pool, tenant_b).await;

        acquire(&pool, tenant_a, host_a, "res-a", "holder-a", 3600)
            .await
            .expect("Failed to acquire lease-a");
        acquire(&pool, tenant_b, host_b, "res-b", "holder-b", 3600)
            .await
            .expect("Failed to acquire lease-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_capability_leases TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see lease-a
        let mut tx = pool.begin().await.expect("Failed to begin tx");
        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");
        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");

        let leases: Vec<(String,)> = sqlx::query_as("SELECT resource FROM roz_capability_leases")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query leases");
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].0, "res-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see lease-b
        let mut tx = pool.begin().await.expect("Failed to begin tx");
        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");
        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_b.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");
        let leases: Vec<(String,)> = sqlx::query_as("SELECT resource FROM roz_capability_leases")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query leases");
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].0, "res-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_capability_leases FROM {test_role}"))
            .execute(&pool)
            .await
            .ok();
        sqlx::query(&format!("REVOKE USAGE ON SCHEMA public FROM {test_role}"))
            .execute(&pool)
            .await
            .ok();
        sqlx::query(&format!("DROP ROLE IF EXISTS {test_role}"))
            .execute(&pool)
            .await
            .ok();
    }
}
