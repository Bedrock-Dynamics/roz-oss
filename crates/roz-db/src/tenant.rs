use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub kind: String,
    pub external_id: Option<String>,
    /// Billing plan slug: "free", "paid", "team", "enterprise".
    /// Column has DEFAULT 'free'; #[sqlx(default)] handles pre-migration rows.
    #[sqlx(default)]
    pub plan: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantMember {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: String,
    pub role: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn create_tenant<'e, E>(executor: E, name: &str, slug: &str, kind: &str) -> Result<Tenant, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, Tenant>("INSERT INTO roz_tenants (name, slug, kind) VALUES ($1, $2, $3) RETURNING *")
        .bind(name)
        .bind(slug)
        .bind(kind)
        .fetch_one(executor)
        .await
}

pub async fn get_tenant<'e, E>(executor: E, id: Uuid) -> Result<Option<Tenant>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, Tenant>("SELECT * FROM roz_tenants WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// Create a tenant with an external ID (e.g., Clerk org / user ID).
pub async fn create_tenant_with_external_id<'e, E>(
    executor: E,
    name: &str,
    slug: &str,
    kind: &str,
    external_id: &str,
) -> Result<Tenant, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, Tenant>(
        "INSERT INTO roz_tenants (name, slug, kind, external_id) VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(name)
    .bind(slug)
    .bind(kind)
    .bind(external_id)
    .fetch_one(executor)
    .await
}

/// Look up a tenant by its external ID (e.g., Clerk `org_id` or `user_id`).
pub async fn get_tenant_by_external_id<'e, E>(executor: E, external_id: &str) -> Result<Option<Tenant>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, Tenant>("SELECT * FROM roz_tenants WHERE external_id = $1")
        .bind(external_id)
        .fetch_optional(executor)
        .await
}

pub async fn add_member<'e, E>(
    executor: E,
    tenant_id: Uuid,
    user_id: &str,
    role: &str,
) -> Result<TenantMember, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TenantMember>(
        "INSERT INTO roz_tenant_members (tenant_id, user_id, role) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(role)
    .fetch_one(executor)
    .await
}

pub async fn list_members<'e, E>(executor: E, tenant_id: Uuid) -> Result<Vec<TenantMember>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TenantMember>("SELECT * FROM roz_tenant_members WHERE tenant_id = $1")
        .bind(tenant_id)
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

    #[tokio::test]
    async fn tenant_crud() {
        let pool = setup().await;
        let slug = format!("test-{}", Uuid::new_v4());

        let tenant = create_tenant(&pool, "Test Org", &slug, "organization")
            .await
            .expect("Failed to create tenant");

        assert_eq!(tenant.name, "Test Org");
        assert_eq!(tenant.slug, slug);
        assert_eq!(tenant.kind, "organization");

        // Read back (bypassing RLS with direct query)
        let found = get_tenant(&pool, tenant.id).await.expect("Failed to get tenant");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Test Org");
    }

    #[tokio::test]
    async fn tenant_members() {
        let pool = setup().await;
        let slug = format!("test-{}", Uuid::new_v4());

        let tenant = create_tenant(&pool, "Member Test Org", &slug, "organization")
            .await
            .expect("Failed to create tenant");

        let member = add_member(&pool, tenant.id, "user_123", "admin")
            .await
            .expect("Failed to add member");
        assert_eq!(member.user_id, "user_123");
        assert_eq!(member.role, "admin");

        let members = list_members(&pool, tenant.id).await.expect("Failed to list members");
        assert_eq!(members.len(), 1);
    }

    #[tokio::test]
    async fn rls_isolation_environments() {
        let pool = setup().await;

        // Create two tenants
        let slug_a = format!("tenant-a-{}", Uuid::new_v4());
        let slug_b = format!("tenant-b-{}", Uuid::new_v4());
        let tenant_a = create_tenant(&pool, "Tenant A", &slug_a, "organization")
            .await
            .expect("Failed to create tenant A");
        let tenant_b = create_tenant(&pool, "Tenant B", &slug_b, "organization")
            .await
            .expect("Failed to create tenant B");

        // Insert environment for tenant A (bypass RLS as superuser)
        sqlx::query("INSERT INTO roz_environments (tenant_id, name, kind) VALUES ($1, $2, $3)")
            .bind(tenant_a.id)
            .bind("Env A")
            .bind("simulation")
            .execute(&pool)
            .await
            .expect("Failed to insert env for tenant A");

        // Insert environment for tenant B
        sqlx::query("INSERT INTO roz_environments (tenant_id, name, kind) VALUES ($1, $2, $3)")
            .bind(tenant_b.id)
            .bind("Env B")
            .bind("hardware")
            .execute(&pool)
            .await
            .expect("Failed to insert env for tenant B");

        // Now test RLS: create a non-superuser role and test isolation
        // Since we're using the postgres superuser (which bypasses RLS),
        // we need to create a test role that respects RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        // Create a test role, grant permissions, and test RLS
        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_environments TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // Use a transaction with SET LOCAL ROLE to test RLS
        let mut tx = pool.begin().await.expect("Failed to begin tx");

        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");

        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_a.id.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");

        let envs: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_environments")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query environments");

        // Tenant A should only see their own environment
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "Env A");

        tx.rollback().await.expect("Failed to rollback");

        // Now check as tenant B
        let mut tx = pool.begin().await.expect("Failed to begin tx");

        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");

        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_b.id.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");

        let envs: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_environments")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query environments");

        // Tenant B should only see their own environment
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "Env B");

        tx.rollback().await.expect("Failed to rollback");

        // Cleanup: drop the test role
        sqlx::query(&format!("REVOKE ALL ON roz_environments FROM {test_role}"))
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
