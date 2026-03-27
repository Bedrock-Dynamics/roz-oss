use sqlx::PgPool;
use uuid::Uuid;

/// Row type matching the `roz_hosts` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct HostRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub host_type: String,
    pub status: String,
    pub capabilities: Vec<String>,
    pub labels: serde_json::Value,
    pub worker_version: Option<String>,
    pub clock_offset_ms: Option<f64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new host and return the created row.
pub async fn create(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    host_type: &str,
    capabilities: &[String],
    labels: &serde_json::Value,
) -> Result<HostRow, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "INSERT INTO roz_hosts (tenant_id, name, host_type, capabilities, labels) \
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(host_type)
    .bind(capabilities)
    .bind(labels)
    .fetch_one(pool)
    .await
}

/// Fetch a single host by primary key, or `None` if not found.
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>("SELECT * FROM roz_hosts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// List hosts for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list(pool: &PgPool, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "SELECT * FROM roz_hosts WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Partially update a host. Only non-`None` fields are changed.
/// Returns `None` when the row does not exist.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    labels: Option<&serde_json::Value>,
) -> Result<Option<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "UPDATE roz_hosts \
         SET name       = COALESCE($2, name), \
             labels     = COALESCE($3, labels), \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(labels)
    .fetch_optional(pool)
    .await
}

/// Delete a host by id. Returns `true` when a row was actually removed.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM roz_hosts WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Update host status (for heartbeat/status changes). Also sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn update_status(pool: &PgPool, id: Uuid, status: &str) -> Result<Option<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "UPDATE roz_hosts \
         SET status     = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(pool)
    .await
}

/// Find hosts that have ALL the required capabilities (array containment: `capabilities @> $1`).
pub async fn list_by_capabilities(
    pool: &PgPool,
    tenant_id: Uuid,
    required_caps: &[String],
) -> Result<Vec<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "SELECT * FROM roz_hosts WHERE tenant_id = $1 AND capabilities @> $2 ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .bind(required_caps)
    .fetch_all(pool)
    .await
}

/// List hosts with `status = 'online'` for a tenant.
pub async fn list_online(pool: &PgPool, tenant_id: Uuid) -> Result<Vec<HostRow>, sqlx::Error> {
    sqlx::query_as::<_, HostRow>(
        "SELECT * FROM roz_hosts WHERE tenant_id = $1 AND status = 'online' ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> PgPool {
        crate::shared_test_pool().await
    }

    async fn create_test_tenant(pool: &PgPool) -> Uuid {
        let slug = format!("test-{}", Uuid::new_v4());
        let tenant = crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant");
        tenant.id
    }

    #[tokio::test]
    async fn create_and_get_host() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps = vec!["gpu".to_string(), "ros2".to_string()];
        let labels = serde_json::json!({"region": "us-west", "tier": "premium"});

        let host = create(&pool, tenant_id, "edge-node-1", "edge", &caps, &labels)
            .await
            .expect("Failed to create host");

        assert_eq!(host.name, "edge-node-1");
        assert_eq!(host.host_type, "edge");
        assert_eq!(host.tenant_id, tenant_id);
        assert_eq!(host.status, "offline");
        assert_eq!(host.capabilities, caps);
        assert_eq!(host.labels, labels);
        assert!(host.worker_version.is_none());
        assert!(host.clock_offset_ms.is_none());

        let fetched = get_by_id(&pool, host.id)
            .await
            .expect("Failed to get host")
            .expect("Host should exist");

        assert_eq!(fetched.id, host.id);
        assert_eq!(fetched.name, "edge-node-1");
        assert_eq!(fetched.capabilities, caps);
        assert_eq!(fetched.labels, labels);
    }

    #[tokio::test]
    async fn list_hosts() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({});

        create(&pool, tenant_id, "host-1", "cloud", &caps, &labels)
            .await
            .expect("Failed to create host-1");
        create(&pool, tenant_id, "host-2", "edge", &caps, &labels)
            .await
            .expect("Failed to create host-2");

        let hosts = list(&pool, tenant_id, 100, 0).await.expect("Failed to list hosts");
        assert!(hosts.len() >= 2, "expected at least 2, got {}", hosts.len());
        assert!(hosts.iter().all(|h| h.tenant_id == tenant_id));

        // Offset past all rows yields empty.
        let page = list(&pool, tenant_id, 10, i64::try_from(hosts.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn update_host() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({"env": "dev"});

        let host = create(&pool, tenant_id, "old-name", "cloud", &caps, &labels)
            .await
            .expect("Failed to create host");

        let updated = update(&pool, host.id, Some("new-name"), None)
            .await
            .expect("Failed to update host")
            .expect("Host should exist");

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.labels, labels); // labels unchanged

        // Update labels only
        let new_labels = serde_json::json!({"env": "prod"});
        let updated2 = update(&pool, host.id, None, Some(&new_labels))
            .await
            .expect("Failed to update labels")
            .expect("Host should exist");

        assert_eq!(updated2.name, "new-name"); // name unchanged
        assert_eq!(updated2.labels, new_labels);
    }

    #[tokio::test]
    async fn delete_host() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({});

        let host = create(&pool, tenant_id, "to-delete", "edge", &caps, &labels)
            .await
            .expect("Failed to create host");

        let deleted = delete(&pool, host.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, host.id).await.expect("Failed to get");
        assert!(gone.is_none());

        // Deleting again returns false (no row affected).
        let again = delete(&pool, host.id).await.expect("Failed to delete again");
        assert!(!again);
    }

    #[tokio::test]
    async fn update_host_status() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({});

        let host = create(&pool, tenant_id, "status-host", "edge", &caps, &labels)
            .await
            .expect("Failed to create host");

        assert_eq!(host.status, "offline");

        let updated = update_status(&pool, host.id, "online")
            .await
            .expect("Failed to update status")
            .expect("Host should exist");

        assert_eq!(updated.status, "online");
        assert!(updated.updated_at >= host.updated_at);

        // Transition to degraded
        let degraded = update_status(&pool, host.id, "degraded")
            .await
            .expect("Failed to update status")
            .expect("Host should exist");

        assert_eq!(degraded.status, "degraded");
    }

    #[tokio::test]
    async fn list_hosts_by_capabilities() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let labels = serde_json::json!({});

        // Host with gpu + ros2
        create(
            &pool,
            tenant_id,
            "gpu-ros2-host",
            "edge",
            &["gpu".to_string(), "ros2".to_string()],
            &labels,
        )
        .await
        .expect("Failed to create gpu-ros2 host");

        // Host with ros2 only
        create(&pool, tenant_id, "ros2-host", "edge", &["ros2".to_string()], &labels)
            .await
            .expect("Failed to create ros2 host");

        // Host with no capabilities
        create(&pool, tenant_id, "bare-host", "cloud", &[], &labels)
            .await
            .expect("Failed to create bare host");

        // Query for gpu: should return only gpu-ros2-host
        let gpu_hosts = list_by_capabilities(&pool, tenant_id, &["gpu".to_string()])
            .await
            .expect("Failed to list by capabilities");
        assert_eq!(gpu_hosts.len(), 1);
        assert_eq!(gpu_hosts[0].name, "gpu-ros2-host");

        // Query for ros2: should return both ros2-capable hosts
        let ros2_hosts = list_by_capabilities(&pool, tenant_id, &["ros2".to_string()])
            .await
            .expect("Failed to list by capabilities");
        assert_eq!(ros2_hosts.len(), 2);

        // Query for gpu + ros2: should return only gpu-ros2-host
        let both_hosts = list_by_capabilities(&pool, tenant_id, &["gpu".to_string(), "ros2".to_string()])
            .await
            .expect("Failed to list by capabilities");
        assert_eq!(both_hosts.len(), 1);
        assert_eq!(both_hosts[0].name, "gpu-ros2-host");

        // Query for nonexistent capability: should return empty
        let empty = list_by_capabilities(&pool, tenant_id, &["lidar".to_string()])
            .await
            .expect("Failed to list by capabilities");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn list_online_hosts() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({});

        let online_host = create(&pool, tenant_id, "online-host", "edge", &caps, &labels)
            .await
            .expect("Failed to create host");
        update_status(&pool, online_host.id, "online")
            .await
            .expect("Failed to set online");

        // This host stays offline (default)
        create(&pool, tenant_id, "offline-host", "cloud", &caps, &labels)
            .await
            .expect("Failed to create host");

        let online = list_online(&pool, tenant_id).await.expect("Failed to list online");
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].name, "online-host");
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let caps: Vec<String> = vec![];
        let labels = serde_json::json!({});

        // Insert as superuser (bypasses RLS)
        create(&pool, tenant_a, "host-a", "edge", &caps, &labels)
            .await
            .expect("Failed to create host-a");
        create(&pool, tenant_b, "host-b", "cloud", &caps, &labels)
            .await
            .expect("Failed to create host-b");

        // Create restricted role to test RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_hosts TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see host-a
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

        let hosts: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_hosts")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query hosts");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].0, "host-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see host-b
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
        let hosts: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_hosts")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query hosts");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].0, "host-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_hosts FROM {test_role}"))
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
