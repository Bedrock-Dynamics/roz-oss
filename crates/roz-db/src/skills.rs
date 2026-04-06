use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// Row type matching the `roz_skills` schema exactly (Phase 4 enhanced).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SkillRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub platform: Vec<String>,
    pub requires_confirmation: bool,
    pub parameters: serde_json::Value,
    pub safety_overrides: Option<serde_json::Value>,
    pub environment_constraints: serde_json::Value,
    pub allowed_tools: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Row type matching the `roz_skill_versions` schema exactly (Phase 4 enhanced).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SkillVersionRow {
    pub id: Uuid,
    pub skill_id: Uuid,
    pub tenant_id: Uuid,
    pub version: String,
    pub content: String,
    pub content_hash: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Skill CRUD
// ---------------------------------------------------------------------------

/// Insert a new skill and return the created row.
pub async fn create(pool: &PgPool, tenant_id: Uuid, name: &str, description: &str) -> Result<SkillRow, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "INSERT INTO roz_skills (tenant_id, name, description) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await
}

/// Fetch a single skill by primary key, or `None` if not found.
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>("SELECT * FROM roz_skills WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// List skills for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list(pool: &PgPool, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "SELECT * FROM roz_skills WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Partially update a skill. Only non-`None` fields are changed.
/// Returns `None` when the row does not exist.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Option<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "UPDATE roz_skills \
         SET name        = COALESCE($2, name), \
             description = COALESCE($3, description), \
             updated_at  = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .fetch_optional(pool)
    .await
}

/// Delete a skill by id. Returns `true` when a row was actually removed.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM roz_skills WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Skill version CRUD
// ---------------------------------------------------------------------------

/// Create a new version for a skill. Derives `tenant_id` from the parent skill
/// to prevent cross-tenant mismatches.
pub async fn create_version(
    pool: &PgPool,
    skill_id: Uuid,
    version: &str,
    content: &str,
) -> Result<SkillVersionRow, sqlx::Error> {
    sqlx::query_as::<_, SkillVersionRow>(
        "INSERT INTO roz_skill_versions (skill_id, tenant_id, version, content) \
         SELECT $1, tenant_id, $2, $3 FROM roz_skills WHERE id = $1 \
         RETURNING *",
    )
    .bind(skill_id)
    .bind(version)
    .bind(content)
    .fetch_one(pool)
    .await
}

/// Get a specific version of a skill.
pub async fn get_version(pool: &PgPool, skill_id: Uuid, version: &str) -> Result<Option<SkillVersionRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillVersionRow>("SELECT * FROM roz_skill_versions WHERE skill_id = $1 AND version = $2")
        .bind(skill_id)
        .bind(version)
        .fetch_optional(pool)
        .await
}

/// List all versions for a skill, ordered by creation time (newest first).
pub async fn list_versions(pool: &PgPool, skill_id: Uuid) -> Result<Vec<SkillVersionRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillVersionRow>(
        "SELECT * FROM roz_skill_versions WHERE skill_id = $1 ORDER BY created_at DESC",
    )
    .bind(skill_id)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Enhanced skill queries (Phase 4)
// ---------------------------------------------------------------------------

/// Create a skill with all Phase 4 metadata fields.
#[allow(clippy::too_many_arguments)]
pub async fn create_with_metadata(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    description: &str,
    kind: &str,
    tags: &[String],
    platform: &[String],
    requires_confirmation: bool,
    parameters: &serde_json::Value,
    safety_overrides: Option<&serde_json::Value>,
    environment_constraints: &serde_json::Value,
    allowed_tools: &[String],
) -> Result<SkillRow, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "INSERT INTO roz_skills \
             (tenant_id, name, description, kind, tags, platform, \
              requires_confirmation, parameters, safety_overrides, \
              environment_constraints, allowed_tools) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(description)
    .bind(kind)
    .bind(tags)
    .bind(platform)
    .bind(requires_confirmation)
    .bind(parameters)
    .bind(safety_overrides)
    .bind(environment_constraints)
    .bind(allowed_tools)
    .fetch_one(pool)
    .await
}

/// List skills filtered by kind (`ai` or `execution`) for a tenant.
pub async fn list_by_kind(
    pool: &PgPool,
    tenant_id: Uuid,
    kind: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "SELECT * FROM roz_skills \
         WHERE tenant_id = $1 AND kind = $2 \
         ORDER BY created_at DESC LIMIT $3 OFFSET $4",
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// List skills that contain a specific tag (uses `@>` array containment).
pub async fn list_by_tag(
    pool: &PgPool,
    tenant_id: Uuid,
    tag: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>(
        "SELECT * FROM roz_skills \
         WHERE tenant_id = $1 AND tags @> ARRAY[$2]::text[] \
         ORDER BY created_at DESC LIMIT $3 OFFSET $4",
    )
    .bind(tenant_id)
    .bind(tag)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Get a skill by name within a tenant, or `None` if not found.
pub async fn get_by_name(pool: &PgPool, tenant_id: Uuid, name: &str) -> Result<Option<SkillRow>, sqlx::Error> {
    sqlx::query_as::<_, SkillRow>("SELECT * FROM roz_skills WHERE tenant_id = $1 AND name = $2")
        .bind(tenant_id)
        .bind(name)
        .fetch_optional(pool)
        .await
}

/// Search skills by name or description using case-insensitive pattern matching.
pub async fn search(
    pool: &PgPool,
    tenant_id: Uuid,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SkillRow>, sqlx::Error> {
    let pattern = format!("%{query}%");
    sqlx::query_as::<_, SkillRow>(
        "SELECT * FROM roz_skills \
         WHERE tenant_id = $1 AND (name ILIKE $2 OR description ILIKE $2) \
         ORDER BY created_at DESC LIMIT $3 OFFSET $4",
    )
    .bind(tenant_id)
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
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
        crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant")
            .id
    }

    #[tokio::test]
    async fn create_and_get_skill() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create(&pool, tenant_id, "pick-and-place", "Picks up objects and places them")
            .await
            .expect("Failed to create skill");

        assert_eq!(skill.tenant_id, tenant_id);
        assert_eq!(skill.name, "pick-and-place");
        assert_eq!(skill.description, "Picks up objects and places them");

        let fetched = get_by_id(&pool, skill.id)
            .await
            .expect("Failed to get skill")
            .expect("Skill should exist");

        assert_eq!(fetched.id, skill.id);
        assert_eq!(fetched.name, "pick-and-place");
    }

    #[tokio::test]
    async fn list_skills() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        create(&pool, tenant_id, "skill-1", "desc-1")
            .await
            .expect("Failed to create skill-1");
        create(&pool, tenant_id, "skill-2", "desc-2")
            .await
            .expect("Failed to create skill-2");

        let skills = list(&pool, tenant_id, 100, 0).await.expect("Failed to list skills");
        assert!(skills.len() >= 2, "expected at least 2, got {}", skills.len());
        assert!(skills.iter().all(|s| s.tenant_id == tenant_id));
    }

    #[tokio::test]
    async fn update_skill() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create(&pool, tenant_id, "old-name", "old-desc")
            .await
            .expect("Failed to create skill");

        let updated = update(&pool, skill.id, Some("new-name"), None)
            .await
            .expect("Failed to update skill")
            .expect("Skill should exist");

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.description, "old-desc"); // unchanged
    }

    #[tokio::test]
    async fn delete_skill() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create(&pool, tenant_id, "to-delete", "gone soon")
            .await
            .expect("Failed to create skill");

        let deleted = delete(&pool, skill.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, skill.id).await.expect("Failed to get");
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn create_and_get_version() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create(&pool, tenant_id, "nav-skill", "Navigation skill")
            .await
            .expect("Failed to create skill");

        let v1 = create_version(&pool, skill.id, "1.0.0", "def navigate(): pass")
            .await
            .expect("Failed to create version");

        assert_eq!(v1.skill_id, skill.id);
        assert_eq!(v1.tenant_id, tenant_id);
        assert_eq!(v1.version, "1.0.0");
        assert_eq!(v1.content, "def navigate(): pass");

        let fetched = get_version(&pool, skill.id, "1.0.0")
            .await
            .expect("Failed to get version")
            .expect("Version should exist");

        assert_eq!(fetched.id, v1.id);
        assert_eq!(fetched.version, "1.0.0");
    }

    #[tokio::test]
    async fn list_skill_versions() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create(&pool, tenant_id, "versioned-skill", "has versions")
            .await
            .expect("Failed to create skill");

        create_version(&pool, skill.id, "1.0.0", "v1 code")
            .await
            .expect("Failed to create v1");
        create_version(&pool, skill.id, "1.1.0", "v1.1 code")
            .await
            .expect("Failed to create v1.1");
        create_version(&pool, skill.id, "2.0.0", "v2 code")
            .await
            .expect("Failed to create v2");

        let versions = list_versions(&pool, skill.id).await.expect("Failed to list versions");
        assert_eq!(versions.len(), 3);
        // Newest first
        assert_eq!(versions[0].version, "2.0.0");
    }

    #[tokio::test]
    async fn rls_tenant_isolation_skills() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;

        create(&pool, tenant_a, "skill-a", "tenant A skill")
            .await
            .expect("Failed to create skill-a");
        create(&pool, tenant_b, "skill-b", "tenant B skill")
            .await
            .expect("Failed to create skill-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_skills TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see skill-a
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

        let skills: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_skills")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].0, "skill-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see skill-b
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
        let skills: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_skills")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].0, "skill-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_skills FROM {test_role}"))
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

    #[tokio::test]
    async fn rls_tenant_isolation_skill_versions() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;

        let skill_a = create(&pool, tenant_a, "sv-skill-a", "a")
            .await
            .expect("Failed to create skill-a");
        let skill_b = create(&pool, tenant_b, "sv-skill-b", "b")
            .await
            .expect("Failed to create skill-b");

        create_version(&pool, skill_a.id, "1.0.0", "code-a")
            .await
            .expect("Failed to create version-a");
        create_version(&pool, skill_b.id, "1.0.0", "code-b")
            .await
            .expect("Failed to create version-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_skill_versions TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see version-a
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

        let versions: Vec<(String,)> = sqlx::query_as("SELECT content FROM roz_skill_versions")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query versions");
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].0, "code-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see version-b
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
        let versions: Vec<(String,)> = sqlx::query_as("SELECT content FROM roz_skill_versions")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query versions");
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].0, "code-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_skill_versions FROM {test_role}"))
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

    // -----------------------------------------------------------------------
    // Phase 4: enhanced skill query tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_with_metadata_full() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let params = serde_json::json!([{"name": "speed", "type": "float"}]);
        let overrides = serde_json::json!({"max_velocity": 1.5});
        let env_constraints = serde_json::json!([{"key": "indoor", "value": true}]);

        let skill = create_with_metadata(
            &pool,
            tenant_id,
            "assembly-task",
            "Assembles parts on a workcell",
            "execution",
            &["manufacturing".to_owned(), "assembly".to_owned()],
            &["ur5e".to_owned()],
            true,
            &params,
            Some(&overrides),
            &env_constraints,
            &["gripper".to_owned(), "camera".to_owned()],
        )
        .await
        .expect("Failed to create skill with metadata");

        assert_eq!(skill.tenant_id, tenant_id);
        assert_eq!(skill.name, "assembly-task");
        assert_eq!(skill.kind, "execution");
        assert_eq!(skill.tags, vec!["manufacturing", "assembly"]);
        assert_eq!(skill.platform, vec!["ur5e"]);
        assert!(skill.requires_confirmation);
        assert_eq!(skill.parameters, params);
        assert_eq!(skill.safety_overrides, Some(overrides));
        assert_eq!(skill.environment_constraints, env_constraints);
        assert_eq!(skill.allowed_tools, vec!["gripper", "camera"]);
    }

    #[tokio::test]
    async fn create_with_metadata_defaults() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let skill = create_with_metadata(
            &pool,
            tenant_id,
            "simple-ai-skill",
            "Uses defaults",
            "ai",
            &[],
            &[],
            false,
            &serde_json::json!([]),
            None,
            &serde_json::json!([]),
            &[],
        )
        .await
        .expect("Failed to create skill with defaults");

        assert_eq!(skill.kind, "ai");
        assert!(skill.tags.is_empty());
        assert!(skill.platform.is_empty());
        assert!(!skill.requires_confirmation);
        assert_eq!(skill.parameters, serde_json::json!([]));
        assert!(skill.safety_overrides.is_none());
        assert!(skill.allowed_tools.is_empty());
    }

    #[tokio::test]
    async fn list_by_kind_filters() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        create_with_metadata(
            &pool,
            tenant_id,
            "ai-skill-1",
            "AI skill",
            "ai",
            &[],
            &[],
            false,
            &serde_json::json!([]),
            None,
            &serde_json::json!([]),
            &[],
        )
        .await
        .expect("Failed to create ai skill");

        create_with_metadata(
            &pool,
            tenant_id,
            "exec-skill-1",
            "Execution skill",
            "execution",
            &[],
            &[],
            false,
            &serde_json::json!([]),
            None,
            &serde_json::json!([]),
            &[],
        )
        .await
        .expect("Failed to create execution skill");

        let ai_skills = list_by_kind(&pool, tenant_id, "ai", 100, 0)
            .await
            .expect("Failed to list ai skills");
        assert!(ai_skills.iter().all(|s| s.kind == "ai"));
        assert!(ai_skills.iter().any(|s| s.name == "ai-skill-1"));

        let exec_skills = list_by_kind(&pool, tenant_id, "execution", 100, 0)
            .await
            .expect("Failed to list execution skills");
        assert!(exec_skills.iter().all(|s| s.kind == "execution"));
        assert!(exec_skills.iter().any(|s| s.name == "exec-skill-1"));
    }

    #[tokio::test]
    async fn list_by_tag_filters() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        create_with_metadata(
            &pool,
            tenant_id,
            "tagged-skill-a",
            "Has navigation tag",
            "ai",
            &["navigation".to_owned(), "outdoor".to_owned()],
            &[],
            false,
            &serde_json::json!([]),
            None,
            &serde_json::json!([]),
            &[],
        )
        .await
        .expect("Failed to create tagged skill a");

        create_with_metadata(
            &pool,
            tenant_id,
            "tagged-skill-b",
            "Has manipulation tag",
            "ai",
            &["manipulation".to_owned()],
            &[],
            false,
            &serde_json::json!([]),
            None,
            &serde_json::json!([]),
            &[],
        )
        .await
        .expect("Failed to create tagged skill b");

        let nav_skills = list_by_tag(&pool, tenant_id, "navigation", 100, 0)
            .await
            .expect("Failed to list by tag");
        assert!(!nav_skills.is_empty());
        assert!(nav_skills.iter().all(|s| s.tags.contains(&"navigation".to_owned())));
        assert!(nav_skills.iter().any(|s| s.name == "tagged-skill-a"));

        let manip_skills = list_by_tag(&pool, tenant_id, "manipulation", 100, 0)
            .await
            .expect("Failed to list by manipulation tag");
        assert!(manip_skills.iter().any(|s| s.name == "tagged-skill-b"));
        assert!(!manip_skills.iter().any(|s| s.name == "tagged-skill-a"));
    }

    #[tokio::test]
    async fn get_by_name_found_and_missing() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        create(&pool, tenant_id, "unique-name-test", "desc")
            .await
            .expect("Failed to create skill");

        let found = get_by_name(&pool, tenant_id, "unique-name-test")
            .await
            .expect("Failed to get by name")
            .expect("Skill should exist");
        assert_eq!(found.name, "unique-name-test");
        assert_eq!(found.tenant_id, tenant_id);

        let missing = get_by_name(&pool, tenant_id, "nonexistent-skill")
            .await
            .expect("Failed to get by name");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn get_by_name_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;

        create(&pool, tenant_a, "shared-name", "tenant A version")
            .await
            .expect("Failed to create skill for tenant A");
        create(&pool, tenant_b, "shared-name", "tenant B version")
            .await
            .expect("Failed to create skill for tenant B");

        let found_a = get_by_name(&pool, tenant_a, "shared-name")
            .await
            .expect("Failed to get by name")
            .expect("Skill should exist for tenant A");
        assert_eq!(found_a.tenant_id, tenant_a);
        assert_eq!(found_a.description, "tenant A version");

        let found_b = get_by_name(&pool, tenant_b, "shared-name")
            .await
            .expect("Failed to get by name")
            .expect("Skill should exist for tenant B");
        assert_eq!(found_b.tenant_id, tenant_b);
        assert_eq!(found_b.description, "tenant B version");
    }

    #[tokio::test]
    async fn search_by_name_and_description() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        create(&pool, tenant_id, "pick-and-place", "Grasps objects and moves them")
            .await
            .expect("Failed to create pick-and-place");
        create(&pool, tenant_id, "navigate-corridor", "Navigates through corridors")
            .await
            .expect("Failed to create navigate-corridor");
        create(&pool, tenant_id, "inspect-weld", "Inspects weld quality with a camera")
            .await
            .expect("Failed to create inspect-weld");

        // Search by name substring
        let results = search(&pool, tenant_id, "navigate", 100, 0)
            .await
            .expect("Failed to search");
        assert!(results.iter().any(|s| s.name == "navigate-corridor"));
        assert!(!results.iter().any(|s| s.name == "pick-and-place"));

        // Search by description substring
        let results = search(&pool, tenant_id, "camera", 100, 0)
            .await
            .expect("Failed to search by description");
        assert!(results.iter().any(|s| s.name == "inspect-weld"));

        // Case insensitivity
        let results = search(&pool, tenant_id, "NAVIGATE", 100, 0)
            .await
            .expect("Failed to search case-insensitive");
        assert!(results.iter().any(|s| s.name == "navigate-corridor"));

        // No results for unrelated query
        let results = search(&pool, tenant_id, "zzz-no-match-zzz", 100, 0)
            .await
            .expect("Failed to search no match");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_respects_pagination() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        for i in 0..5 {
            create(
                &pool,
                tenant_id,
                &format!("paginated-skill-{i}"),
                "Paginated test skill",
            )
            .await
            .unwrap_or_else(|_| panic!("Failed to create paginated-skill-{i}"));
        }

        let page1 = search(&pool, tenant_id, "paginated-skill", 2, 0)
            .await
            .expect("Failed to search page 1");
        assert_eq!(page1.len(), 2);

        let page2 = search(&pool, tenant_id, "paginated-skill", 2, 2)
            .await
            .expect("Failed to search page 2");
        assert_eq!(page2.len(), 2);

        // No overlap
        let page1_ids: Vec<Uuid> = page1.iter().map(|s| s.id).collect();
        assert!(page2.iter().all(|s| !page1_ids.contains(&s.id)));
    }
}
