//! CRUD operations for embodiment JSONB data stored on `roz_hosts`.

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
pub async fn get_by_host_id<'e, E>(executor: E, host_id: Uuid) -> Result<Option<EmbodimentRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EmbodimentRow>(
        "SELECT id, tenant_id, embodiment_model, embodiment_runtime FROM roz_hosts WHERE id = $1",
    )
    .bind(host_id)
    .fetch_optional(executor)
    .await
}

/// Upsert embodiment data for a host. Sets model and optionally runtime.
/// Only updates if the host exists.
pub async fn upsert<'e, E>(
    executor: E,
    host_id: Uuid,
    model: &serde_json::Value,
    runtime: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
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
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Conditionally upsert embodiment data for a host.
///
/// Skips the write when the incoming model's `model_digest` matches the stored
/// value (atomic in a single UPDATE query). Returns `true` when a write
/// occurred, `false` when skipped because the digest was unchanged.
pub async fn conditional_upsert<'e, E>(
    executor: E,
    host_id: Uuid,
    model: &serde_json::Value,
    runtime: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "UPDATE roz_hosts \
         SET embodiment_model = $2, \
             embodiment_runtime = $3, \
             updated_at = now() \
         WHERE id = $1 \
           AND (embodiment_model IS NULL \
                OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest')",
    )
    .bind(host_id)
    .bind(model)
    .bind(runtime)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Conditionally upsert embodiment data for a host, writing when EITHER the
/// model digest changes OR new runtime data is provided.
///
/// - When `model` changes (different `model_digest`): writes model + runtime.
/// - When `model` is unchanged but `runtime` is provided: writes runtime only
///   (detected via `combined_digest` field on the runtime JSON).
/// - When `model` is unchanged and `runtime` is None: skips (returns false).
///
/// Returns `true` when a write occurred, `false` when skipped.
pub async fn conditional_upsert_or_runtime<'e, E>(
    executor: E,
    host_id: Uuid,
    model: &serde_json::Value,
    runtime: Option<&serde_json::Value>,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    // If runtime is provided, also check runtime's combined_digest to detect
    // calibration-only changes. Use COALESCE so if runtime is NULL the condition
    // evaluates to model-digest check only (original behavior).
    let result = sqlx::query(
        "UPDATE roz_hosts \
         SET embodiment_model = CASE \
               WHEN (embodiment_model IS NULL \
                     OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest') \
               THEN $2 \
               ELSE embodiment_model \
             END, \
             embodiment_runtime = CASE \
               WHEN (embodiment_model IS NULL \
                     OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest') \
               THEN $3 \
               ELSE COALESCE($3, embodiment_runtime) \
             END, \
             updated_at = now() \
         WHERE id = $1 \
           AND ( \
             embodiment_model IS NULL \
             OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest' \
             OR ($3 IS NOT NULL \
                 AND (embodiment_runtime IS NULL \
                      OR embodiment_runtime->>'combined_digest' IS DISTINCT FROM $3->>'combined_digest')) \
           )",
    )
    .bind(host_id)
    .bind(model)
    .bind(runtime)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
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
        let tenant = crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant");
        tenant.id
    }

    #[tokio::test]
    async fn upsert_and_get_model() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "emb-host-1", "edge", &[], &serde_json::json!({}))
            .await
            .expect("Failed to create host");

        let model = serde_json::json!({"model_id": "test-model", "joints": []});
        let updated = upsert(&pool, host.id, &model, None).await.expect("Failed to upsert");
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
        let host = crate::hosts::create(&pool, tenant_id, "emb-host-2", "edge", &[], &serde_json::json!({}))
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
        let row = get_by_host_id(&pool, missing_id).await.expect("Failed to get");
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
    async fn conditional_upsert_or_runtime_writes_on_new_runtime() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(&pool, tenant_id, "cond-rt-host-1", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");

        // First upload: model + runtime v1
        let model = serde_json::json!({"model_digest": "abc123", "joints": []});
        let runtime_v1 = serde_json::json!({"combined_digest": "rt-v1", "calibration": {}});
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model, Some(&runtime_v1))
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(wrote, "first upload should write");

        // Second upload: same model, same runtime -- should skip
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model, Some(&runtime_v1))
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(!wrote, "identical model+runtime should skip");

        // Third upload: same model, new runtime (calibration-only change) -- MUST write
        let runtime_v2 = serde_json::json!({"combined_digest": "rt-v2", "calibration": {"calibration_id": "new"}});
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model, Some(&runtime_v2))
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(wrote, "calibration-only change (new runtime digest) must write");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(
            row.embodiment_runtime.as_ref().unwrap()["combined_digest"],
            "rt-v2",
            "DB must store the new runtime"
        );
    }

    #[tokio::test]
    async fn conditional_upsert_clears_runtime_when_model_changes_and_runtime_is_none() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(
            &pool,
            tenant_id,
            "cond-host-clear-1",
            "edge",
            &[],
            &serde_json::json!({}),
        )
        .await
        .expect("create host");

        // Seed: model v1 with an existing runtime attached.
        let model_v1 = serde_json::json!({"model_digest": "abc123", "joints": []});
        let runtime_v1 = serde_json::json!({"combined_digest": "rt-v1", "calibration": {}});
        let wrote = conditional_upsert(&pool, host.id, &model_v1, Some(&runtime_v1))
            .await
            .expect("conditional_upsert");
        assert!(wrote);

        // New model digest, no runtime supplied -- runtime must be cleared
        // because the stale runtime belongs to the old model.
        let model_v2 = serde_json::json!({"model_digest": "def456", "joints": [{"name": "j1"}]});
        let wrote = conditional_upsert(&pool, host.id, &model_v2, None)
            .await
            .expect("conditional_upsert");
        assert!(wrote, "changed digest should write");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(row.embodiment_model.as_ref().unwrap()["model_digest"], "def456");
        assert!(
            row.embodiment_runtime.is_none(),
            "runtime must be cleared when model changes and no new runtime is provided, got {:?}",
            row.embodiment_runtime
        );
    }

    #[tokio::test]
    async fn conditional_upsert_or_runtime_clears_runtime_when_model_changes_and_runtime_is_none() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(
            &pool,
            tenant_id,
            "cond-host-clear-2",
            "edge",
            &[],
            &serde_json::json!({}),
        )
        .await
        .expect("create host");

        // Seed: model v1 with an existing runtime attached.
        let model_v1 = serde_json::json!({"model_digest": "abc123", "joints": []});
        let runtime_v1 = serde_json::json!({"combined_digest": "rt-v1", "calibration": {}});
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model_v1, Some(&runtime_v1))
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(wrote);

        // New model digest, no runtime supplied -- runtime must be cleared.
        let model_v2 = serde_json::json!({"model_digest": "def456", "joints": [{"name": "j1"}]});
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model_v2, None)
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(wrote, "changed digest should write");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(row.embodiment_model.as_ref().unwrap()["model_digest"], "def456");
        assert!(
            row.embodiment_runtime.is_none(),
            "runtime must be cleared when model changes and no new runtime is provided, got {:?}",
            row.embodiment_runtime
        );
    }

    #[tokio::test]
    async fn conditional_upsert_or_runtime_preserves_runtime_when_model_unchanged_and_runtime_is_none() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host = crate::hosts::create(
            &pool,
            tenant_id,
            "cond-host-clear-3",
            "edge",
            &[],
            &serde_json::json!({}),
        )
        .await
        .expect("create host");

        // Seed: model + runtime
        let model = serde_json::json!({"model_digest": "abc123", "joints": []});
        let runtime = serde_json::json!({"combined_digest": "rt-v1", "calibration": {}});
        conditional_upsert_or_runtime(&pool, host.id, &model, Some(&runtime))
            .await
            .unwrap();

        // Same model, no runtime -- should skip (and therefore preserve runtime).
        let wrote = conditional_upsert_or_runtime(&pool, host.id, &model, None)
            .await
            .expect("conditional_upsert_or_runtime");
        assert!(!wrote, "unchanged model + None runtime should skip");

        let row = get_by_host_id(&pool, host.id).await.unwrap().unwrap();
        assert_eq!(
            row.embodiment_runtime.as_ref().unwrap()["combined_digest"],
            "rt-v1",
            "runtime must be preserved when model is unchanged and no new runtime is provided"
        );
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
