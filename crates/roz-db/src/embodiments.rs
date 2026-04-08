//! CRUD operations for embodiment JSONB data stored on `roz_hosts`.

use sqlx::PgPool;
use uuid::Uuid;

/// Row type for embodiment data queries. Contains host identity + JSONB columns.
#[derive(Debug, sqlx::FromRow)]
pub struct EmbodimentRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub embodiment_model: Option<serde_json::Value>,
    pub embodiment_runtime: Option<serde_json::Value>,
}

/// Fetch embodiment data for a host by its primary key.
pub async fn get_by_host_id(pool: &PgPool, host_id: Uuid) -> Result<Option<EmbodimentRow>, sqlx::Error> {
    sqlx::query_as::<_, EmbodimentRow>(
        "SELECT id, tenant_id, embodiment_model, embodiment_runtime FROM roz_hosts WHERE id = $1",
    )
    .bind(host_id)
    .fetch_optional(pool)
    .await
}

/// Upsert embodiment data for a host. Sets model and optionally runtime.
/// Only updates if the host exists.
pub async fn upsert(
    pool: &PgPool,
    host_id: Uuid,
    model: &serde_json::Value,
    runtime: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE roz_hosts \
         SET embodiment_model = $2, \
             embodiment_runtime = COALESCE($3, embodiment_runtime), \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(host_id)
    .bind(model)
    .bind(runtime)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Conditionally upsert embodiment data for a host.
///
/// Skips the write when the incoming model's `model_digest` matches the stored
/// value (atomic in a single UPDATE query). Returns `true` when a write
/// occurred, `false` when skipped because the digest was unchanged.
pub async fn conditional_upsert(
    pool: &PgPool,
    host_id: Uuid,
    model: &serde_json::Value,
    runtime: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE roz_hosts \
         SET embodiment_model = $2, \
             embodiment_runtime = COALESCE($3, embodiment_runtime), \
             updated_at = now() \
         WHERE id = $1 \
           AND (embodiment_model IS NULL \
                OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest')",
    )
    .bind(host_id)
    .bind(model)
    .bind(runtime)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
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
    async fn upsert_and_get_model() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(
            &pool,
            tenant_id,
            "emb-host-1",
            "edge",
            &[],
            &serde_json::json!({}),
        )
        .await
        .expect("Failed to create host");

        let model = serde_json::json!({"model_id": "test-model", "joints": []});
        let updated = upsert(&pool, host.id, &model, None)
            .await
            .expect("Failed to upsert");
        assert!(updated);

        let row = get_by_host_id(&pool, host.id)
            .await
            .expect("Failed to get")
            .expect("Row should exist");

        assert_eq!(row.id, host.id);
        assert_eq!(row.tenant_id, tenant_id);
        assert_eq!(row.embodiment_model, Some(model));
        assert!(row.embodiment_runtime.is_none());
    }

    #[tokio::test]
    async fn upsert_with_runtime() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(
            &pool,
            tenant_id,
            "emb-host-2",
            "edge",
            &[],
            &serde_json::json!({}),
        )
        .await
        .expect("Failed to create host");

        let model = serde_json::json!({"model_id": "test-model"});
        let runtime = serde_json::json!({"combined_digest": "abc123"});
        let updated = upsert(&pool, host.id, &model, Some(&runtime))
            .await
            .expect("Failed to upsert");
        assert!(updated);

        let row = get_by_host_id(&pool, host.id)
            .await
            .expect("Failed to get")
            .expect("Row should exist");

        assert_eq!(row.embodiment_model, Some(model));
        assert_eq!(row.embodiment_runtime, Some(runtime));
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_host() {
        let pool = setup().await;
        let missing_id = Uuid::new_v4();
        let row = get_by_host_id(&pool, missing_id)
            .await
            .expect("Failed to get");
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn conditional_upsert_first_upload() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "cond-host-1", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");

        let model = serde_json::json!({"model_digest": "abc123", "joints": []});
        let wrote = conditional_upsert(&pool, host.id, &model, None)
            .await
            .expect("conditional_upsert");
        assert!(wrote, "first upload should write");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(row.embodiment_model.as_ref().unwrap()["model_digest"], "abc123");
    }

    #[tokio::test]
    async fn conditional_upsert_skips_identical_digest() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "cond-host-2", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");

        let model = serde_json::json!({"model_digest": "abc123", "joints": []});
        conditional_upsert(&pool, host.id, &model, None).await.unwrap();

        // Second upload with same digest -- should skip
        let wrote = conditional_upsert(&pool, host.id, &model, None)
            .await
            .expect("conditional_upsert");
        assert!(!wrote, "identical digest should skip write");
    }

    #[tokio::test]
    async fn conditional_upsert_writes_on_changed_digest() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "cond-host-3", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");

        let model_v1 = serde_json::json!({"model_digest": "abc123", "joints": []});
        conditional_upsert(&pool, host.id, &model_v1, None).await.unwrap();

        let model_v2 = serde_json::json!({"model_digest": "def456", "joints": [{"name": "j1"}]});
        let wrote = conditional_upsert(&pool, host.id, &model_v2, None)
            .await
            .expect("conditional_upsert");
        assert!(wrote, "changed digest should write");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(row.embodiment_model.as_ref().unwrap()["model_digest"], "def456");
    }

    #[tokio::test]
    async fn conditional_upsert_writes_when_no_digest_field() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "cond-host-4", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");

        // Seed with a model missing model_digest (legacy data)
        let legacy = serde_json::json!({"joints": []});
        upsert(&pool, host.id, &legacy, None).await.unwrap();

        // Upload with digest should write (IS DISTINCT FROM treats null != "abc")
        let model = serde_json::json!({"model_digest": "abc123", "joints": []});
        let wrote = conditional_upsert(&pool, host.id, &model, None)
            .await
            .expect("conditional_upsert");
        assert!(wrote, "missing digest field should write");
    }
}
