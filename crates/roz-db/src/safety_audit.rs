use uuid::Uuid;

/// Row type matching the `roz_safety_audit_log` schema exactly.
/// This table is append-only (INSERT only — UPDATE/DELETE denied by DB).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SafetyAuditRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub event_type: String,
    pub severity: String,
    pub source: String,
    pub details: serde_json::Value,
    pub host_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub policy_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Append a new safety audit event. This table is append-only.
#[allow(clippy::too_many_arguments)]
pub async fn append<'e, E>(
    executor: E,
    tenant_id: Uuid,
    event_type: &str,
    severity: &str,
    source: &str,
    details: &serde_json::Value,
    host_id: Option<Uuid>,
    task_id: Option<Uuid>,
    policy_id: Option<Uuid>,
) -> Result<SafetyAuditRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyAuditRow>(
        "INSERT INTO roz_safety_audit_log \
         (tenant_id, event_type, severity, source, details, host_id, task_id, policy_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *",
    )
    .bind(tenant_id)
    .bind(event_type)
    .bind(severity)
    .bind(source)
    .bind(details)
    .bind(host_id)
    .bind(task_id)
    .bind(policy_id)
    .fetch_one(executor)
    .await
}

/// List safety audit events for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list<'e, E>(
    executor: E,
    tenant_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<SafetyAuditRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyAuditRow>(
        "SELECT * FROM roz_safety_audit_log WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// List safety audit events filtered by severity for a tenant.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list_by_severity<'e, E>(
    executor: E,
    tenant_id: Uuid,
    severity: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SafetyAuditRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyAuditRow>(
        "SELECT * FROM roz_safety_audit_log \
         WHERE tenant_id = $1 AND severity = $2 \
         ORDER BY created_at DESC LIMIT $3 OFFSET $4",
    )
    .bind(tenant_id)
    .bind(severity)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
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

    #[tokio::test]
    async fn append_and_list() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let details = serde_json::json!({"reason": "speed limit exceeded"});

        let event = append(
            &pool,
            tenant_id,
            "speed_violation",
            "warning",
            "safety-daemon",
            &details,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to append audit event");

        assert_eq!(event.tenant_id, tenant_id);
        assert_eq!(event.event_type, "speed_violation");
        assert_eq!(event.severity, "warning");
        assert_eq!(event.source, "safety-daemon");
        assert_eq!(event.details, details);
        assert!(event.host_id.is_none());
        assert!(event.task_id.is_none());
        assert!(event.policy_id.is_none());

        let events = list(&pool, tenant_id, 100, 0).await.expect("Failed to list events");
        assert!(!events.is_empty());
        assert!(events.iter().all(|e| e.tenant_id == tenant_id));
        assert!(events.iter().any(|e| e.id == event.id));
    }

    #[tokio::test]
    async fn list_by_severity_filter() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let details = serde_json::json!({});

        append(
            &pool,
            tenant_id,
            "info-event",
            "info",
            "daemon",
            &details,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to append info event");
        append(
            &pool,
            tenant_id,
            "crit-event",
            "critical",
            "daemon",
            &details,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to append critical event");
        append(
            &pool,
            tenant_id,
            "emergency-event",
            "emergency",
            "daemon",
            &details,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to append emergency event");

        let critical = list_by_severity(&pool, tenant_id, "critical", 100, 0)
            .await
            .expect("Failed to list by severity");
        assert!(!critical.is_empty());
        assert!(critical.iter().all(|e| e.severity == "critical"));
        assert!(critical.iter().all(|e| e.tenant_id == tenant_id));

        let info = list_by_severity(&pool, tenant_id, "info", 100, 0)
            .await
            .expect("Failed to list info events");
        assert!(!info.is_empty());
        assert!(info.iter().all(|e| e.severity == "info"));
    }

    #[tokio::test]
    async fn append_with_optional_ids() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = Some(Uuid::new_v4());
        let task_id = Some(Uuid::new_v4());
        let policy_id = Some(Uuid::new_v4());

        let event = append(
            &pool,
            tenant_id,
            "e-stop",
            "emergency",
            "watchdog",
            &serde_json::json!({"action": "halt"}),
            host_id,
            task_id,
            policy_id,
        )
        .await
        .expect("Failed to append event with ids");

        assert_eq!(event.host_id, host_id);
        assert_eq!(event.task_id, task_id);
        assert_eq!(event.policy_id, policy_id);
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let details = serde_json::json!({});

        append(&pool, tenant_a, "event-a", "info", "src-a", &details, None, None, None)
            .await
            .expect("Failed to append event-a");
        append(
            &pool, tenant_b, "event-b", "warning", "src-b", &details, None, None, None,
        )
        .await
        .expect("Failed to append event-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_safety_audit_log TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see event-a
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

        let events: Vec<(String,)> = sqlx::query_as("SELECT event_type FROM roz_safety_audit_log")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query audit events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "event-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see event-b
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
        let events: Vec<(String,)> = sqlx::query_as("SELECT event_type FROM roz_safety_audit_log")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query audit events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "event-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_safety_audit_log FROM {test_role}"))
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
