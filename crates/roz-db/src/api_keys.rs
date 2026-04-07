use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub scopes: Vec<String>,
    pub created_by: String,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct CreateApiKeyResult {
    pub api_key: ApiKey,
    pub full_key: String,
}

fn generate_key() -> (String, String, String) {
    let mut key_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let full_key = format!("roz_sk_{}", URL_SAFE_NO_PAD.encode(key_bytes));
    let key_prefix = full_key[..16].to_string();
    let key_hash = hex::encode(Sha256::digest(full_key.as_bytes()));
    (full_key, key_prefix, key_hash)
}

fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

pub async fn create_api_key(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    scopes: &[String],
    created_by: &str,
) -> Result<CreateApiKeyResult, sqlx::Error> {
    let (full_key, key_prefix, key_hash) = generate_key();

    let api_key = sqlx::query_as::<_, ApiKey>(
        "INSERT INTO roz_api_keys (tenant_id, name, key_prefix, key_hash, scopes, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(&key_prefix)
    .bind(&key_hash)
    .bind(scopes)
    .bind(created_by)
    .fetch_one(pool)
    .await?;

    Ok(CreateApiKeyResult { api_key, full_key })
}

pub async fn list_api_keys(pool: &PgPool, tenant_id: Uuid) -> Result<Vec<ApiKey>, sqlx::Error> {
    sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM roz_api_keys WHERE tenant_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

pub async fn verify_api_key(pool: &PgPool, key: &str) -> Result<Option<ApiKey>, sqlx::Error> {
    let key_hash = hash_key(key);
    let key_prefix = &key[..std::cmp::min(16, key.len())];

    sqlx::query_as::<_, ApiKey>(
        "UPDATE roz_api_keys \
         SET last_used_at = now() \
         WHERE key_prefix = $1 AND key_hash = $2 AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > now()) \
         RETURNING *",
    )
    .bind(key_prefix)
    .bind(key_hash)
    .fetch_optional(pool)
    .await
}

pub async fn revoke_api_key(pool: &PgPool, id: Uuid, tenant_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE roz_api_keys SET revoked_at = now() WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(tenant_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn rotate_api_key(
    pool: &PgPool,
    id: Uuid,
    tenant_id: Uuid,
) -> Result<Option<CreateApiKeyResult>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Find and revoke old key atomically
    let old_key = sqlx::query_as::<_, ApiKey>(
        "UPDATE roz_api_keys SET revoked_at = now() \
         WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL \
         RETURNING *",
    )
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(old_key) = old_key else {
        return Ok(None);
    };

    // Generate new key
    let (full_key, key_prefix, key_hash) = generate_key();

    let api_key = sqlx::query_as::<_, ApiKey>(
        "INSERT INTO roz_api_keys (tenant_id, name, key_prefix, key_hash, scopes, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(old_key.tenant_id)
    .bind(&old_key.name)
    .bind(&key_prefix)
    .bind(&key_hash)
    .bind(&old_key.scopes)
    .bind(&old_key.created_by)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Some(CreateApiKeyResult { api_key, full_key }))
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
    async fn api_key_create_and_verify() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let result = create_api_key(&pool, tenant_id, "My Key", &["read:tasks".to_string()], "user_123")
            .await
            .expect("Failed to create API key");

        assert_eq!(result.api_key.name, "My Key");
        assert!(result.full_key.starts_with("roz_sk_"));
        assert_eq!(result.api_key.key_prefix, &result.full_key[..16]);

        // Verify the key
        let verified = verify_api_key(&pool, &result.full_key)
            .await
            .expect("Failed to verify API key");
        assert!(verified.is_some());
        let verified = verified.unwrap();
        assert_eq!(verified.id, result.api_key.id);
        assert!(verified.last_used_at.is_some(), "verify should update last_used_at");
    }

    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn api_key_list_excludes_revoked() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let key1 = create_api_key(&pool, tenant_id, "Key 1", &[], "user_123")
            .await
            .expect("Failed to create key 1");
        let _key2 = create_api_key(&pool, tenant_id, "Key 2", &[], "user_123")
            .await
            .expect("Failed to create key 2");

        // List shows both
        let keys = list_api_keys(&pool, tenant_id).await.expect("Failed to list keys");
        assert_eq!(keys.len(), 2);

        // Revoke key 1
        revoke_api_key(&pool, key1.api_key.id, tenant_id)
            .await
            .expect("Failed to revoke key");

        // List shows only key 2
        let keys = list_api_keys(&pool, tenant_id)
            .await
            .expect("Failed to list keys after revoke");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "Key 2");
    }

    #[tokio::test]
    async fn api_key_revoke_makes_verify_fail() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let result = create_api_key(&pool, tenant_id, "Revoke Test", &[], "user_123")
            .await
            .expect("Failed to create key");

        // Verify works
        let verified = verify_api_key(&pool, &result.full_key)
            .await
            .expect("verify before revoke");
        assert!(verified.is_some());

        // Revoke
        let revoked = revoke_api_key(&pool, result.api_key.id, tenant_id)
            .await
            .expect("Failed to revoke");
        assert!(revoked);

        // Verify fails
        let verified = verify_api_key(&pool, &result.full_key)
            .await
            .expect("verify after revoke");
        assert!(verified.is_none());
    }

    #[tokio::test]
    async fn api_key_rotate() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let original = create_api_key(&pool, tenant_id, "Rotate Test", &["admin".to_string()], "user_123")
            .await
            .expect("Failed to create key");

        let rotated = rotate_api_key(&pool, original.api_key.id, tenant_id)
            .await
            .expect("Failed to rotate key")
            .expect("Expected Some");

        // Old key no longer works
        let old_verified = verify_api_key(&pool, &original.full_key).await.expect("verify old key");
        assert!(old_verified.is_none());

        // New key works
        let new_verified = verify_api_key(&pool, &rotated.full_key).await.expect("verify new key");
        assert!(new_verified.is_some());

        // Metadata preserved
        assert_eq!(rotated.api_key.name, "Rotate Test");
        assert_eq!(rotated.api_key.scopes, vec!["admin"]);
    }

    #[tokio::test]
    async fn api_key_full_key_only_on_create() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;

        let result = create_api_key(&pool, tenant_id, "Secret Test", &[], "user_123")
            .await
            .expect("Failed to create key");

        // full_key is returned on create
        assert!(result.full_key.starts_with("roz_sk_"));

        // Listing only returns key_prefix, not the full key
        let keys = list_api_keys(&pool, tenant_id).await.expect("Failed to list keys");
        assert_eq!(keys.len(), 1);
        // key_hash is stored but the full key is not recoverable from the DB
        assert_eq!(keys[0].key_prefix.len(), 16);
        assert_ne!(keys[0].key_hash, result.full_key); // hash != key
    }
}
