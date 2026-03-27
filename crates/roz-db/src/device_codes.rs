use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceCodeRow {
    pub device_code: String,
    pub user_code: String,
    pub user_id: Option<String>,
    pub tenant_id: Option<Uuid>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new device authorization code.
pub async fn create_device_code(
    pool: &PgPool,
    device_code: &str,
    user_code: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<DeviceCodeRow, sqlx::Error> {
    sqlx::query_as::<_, DeviceCodeRow>(
        "INSERT INTO roz_device_codes (device_code, user_code, expires_at) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(device_code)
    .bind(user_code)
    .bind(expires_at)
    .fetch_one(pool)
    .await
}

/// Fetch a device code row by its opaque device code.
pub async fn get_by_device_code(pool: &PgPool, device_code: &str) -> Result<Option<DeviceCodeRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceCodeRow>("SELECT * FROM roz_device_codes WHERE device_code = $1")
        .bind(device_code)
        .fetch_optional(pool)
        .await
}

/// Fetch a device code row by the human-readable user code.
pub async fn get_by_user_code(pool: &PgPool, user_code: &str) -> Result<Option<DeviceCodeRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceCodeRow>("SELECT * FROM roz_device_codes WHERE user_code = $1")
        .bind(user_code)
        .fetch_optional(pool)
        .await
}

/// Mark a device code as completed, linking it to the authenticated user.
pub async fn complete_device_code(
    pool: &PgPool,
    user_code: &str,
    user_id: &str,
    tenant_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE roz_device_codes SET user_id = $1, tenant_id = $2, completed_at = now() \
         WHERE user_code = $3 AND completed_at IS NULL AND expires_at > now()",
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(user_code)
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

    #[tokio::test]
    async fn create_and_get_device_code() {
        let pool = setup().await;

        let device_code = format!("dc_{}", Uuid::new_v4());
        let user_code = format!("CG{}", &Uuid::new_v4().to_string()[..6]).to_uppercase();
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);

        let row = create_device_code(&pool, &device_code, &user_code, expires_at)
            .await
            .expect("Failed to create device code");

        assert_eq!(row.device_code, device_code);
        assert_eq!(row.user_code, user_code);
        assert!(row.user_id.is_none());
        assert!(row.tenant_id.is_none());
        assert!(row.completed_at.is_none());

        // Retrieve by device_code
        let fetched = get_by_device_code(&pool, &device_code)
            .await
            .expect("Failed to get by device_code");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().user_code, *user_code);

        // Non-existent device_code returns None
        let missing = get_by_device_code(&pool, "nonexistent").await.expect("Failed to query");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn get_by_user_code_works() {
        let pool = setup().await;

        let device_code = format!("dc_{}", Uuid::new_v4());
        let user_code = format!("UC{}", &Uuid::new_v4().to_string()[..6]).to_uppercase();
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);

        create_device_code(&pool, &device_code, &user_code, expires_at)
            .await
            .expect("Failed to create device code");

        let fetched = get_by_user_code(&pool, &user_code)
            .await
            .expect("Failed to get by user_code");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().device_code, device_code);

        // Non-existent user_code returns None
        let missing = get_by_user_code(&pool, "ZZZZ-ZZZZ").await.expect("Failed to query");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn complete_device_code_marks_row() {
        let pool = setup().await;

        let device_code = format!("dc_{}", Uuid::new_v4());
        let user_code = format!("CMP{}", &Uuid::new_v4().to_string()[..5]).to_uppercase();
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);

        create_device_code(&pool, &device_code, &user_code, expires_at)
            .await
            .expect("Failed to create device code");

        let tenant_id = Uuid::new_v4();
        let completed = complete_device_code(&pool, &user_code, "user_abc", tenant_id)
            .await
            .expect("Failed to complete device code");
        assert!(completed);

        // Verify the row is updated
        let row = get_by_device_code(&pool, &device_code)
            .await
            .expect("Failed to get device code")
            .expect("row should exist");
        assert_eq!(row.user_id.as_deref(), Some("user_abc"));
        assert_eq!(row.tenant_id, Some(tenant_id));
        assert!(row.completed_at.is_some());

        // Completing again should fail (already completed)
        let again = complete_device_code(&pool, &user_code, "user_xyz", Uuid::new_v4())
            .await
            .expect("Failed to attempt second complete");
        assert!(!again);
    }
}
