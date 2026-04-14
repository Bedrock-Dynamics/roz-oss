pub mod activity_events;
pub mod agent_sessions;
pub mod api_keys;
pub mod commands;
pub mod device_codes;
pub mod device_trust;
pub mod embodiments;
pub mod environments;
pub mod hosts;
pub mod leases;
pub mod message_feedback;
pub mod provenance;
pub mod safety_audit;
pub mod safety_policies;
pub mod session_turns;
pub mod skills;
pub mod streams;
pub mod tasks;
pub mod tenant;
pub mod triggers;
pub mod usage;

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

fn parse_database_max_connections(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(10)
}

fn database_max_connections() -> u32 {
    parse_database_max_connections(std::env::var("ROZ_DB_MAX_CONNECTIONS").ok().as_deref())
}

fn parse_env_duration_secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default),
    )
}

pub async fn create_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let max_connections = database_max_connections();
    let acquire_timeout = parse_env_duration_secs("ROZ_DB_ACQUIRE_TIMEOUT_SECS", 5);
    let idle_timeout = parse_env_duration_secs("ROZ_DB_IDLE_TIMEOUT_SECS", 300);
    let max_lifetime = parse_env_duration_secs("ROZ_DB_MAX_LIFETIME_SECS", 1800);

    tracing::info!(
        max_connections,
        acquire_timeout_secs = acquire_timeout.as_secs(),
        idle_timeout_secs = idle_timeout.as_secs(),
        max_lifetime_secs = max_lifetime.as_secs(),
        "configuring database pool"
    );

    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(acquire_timeout)
        .idle_timeout(idle_timeout)
        .max_lifetime(max_lifetime)
        .connect(database_url)
        .await
}

pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}

/// Set the RLS tenant context for subsequent queries.
///
/// **Must be called within a transaction** for the setting to persist
/// across queries. When called on a bare pool, the setting is scoped
/// to a single implicit transaction and will not carry over.
pub async fn set_tenant_context<'e, E>(executor: E, tenant_id: &uuid::Uuid) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query("SELECT set_config('rls.tenant_id', $1::text, true)")
        .bind(tenant_id.to_string())
        .execute(executor)
        .await?;
    Ok(())
}

/// Returns a fresh connection pool for tests, running migrations exactly once.
///
/// Each `#[tokio::test]` creates its own single-threaded tokio runtime, so
/// connection pools cannot be shared (connections are tied to the runtime
/// that created them). Instead, we create a fresh pool per test but use a
/// `std::sync::Mutex` to ensure migrations run only once.
#[cfg(test)]
pub(crate) async fn shared_test_pool() -> PgPool {
    use std::sync::Mutex;

    static MIGRATED: Mutex<bool> = Mutex::new(false);

    let url = roz_test::pg_url().await;
    let pool = create_pool(url).await.expect("Failed to create test pool");

    // Run migrations exactly once. Holding a sync Mutex across .await is safe
    // because each #[tokio::test] runs on its own dedicated thread — no risk
    // of deadlock or blocking other async tasks.
    #[allow(clippy::await_holding_lock)]
    {
        let mut done = MIGRATED.lock().expect("migration lock poisoned");
        if !*done {
            run_migrations(&pool).await.expect("Failed to run test migrations");
            *done = true;
        }
    }

    pool
}

#[cfg(test)]
fn is_retryable_public_schema_grant_error(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(database_error) => {
            database_error.code().as_deref() == Some("XX000")
                && database_error.message().contains("tuple concurrently updated")
        }
        _ => false,
    }
}

/// Grant `USAGE` on the `public` schema to a transient test role.
///
/// Parallel RLS tests occasionally race while Postgres updates the schema ACL,
/// surfacing `XX000 tuple concurrently updated`. Retry that transient case so
/// the isolation tests remain deterministic under workspace-wide concurrency.
#[cfg(test)]
pub(crate) async fn grant_public_schema_usage_for_test_role(pool: &PgPool, test_role: &str) -> Result<(), sqlx::Error> {
    const MAX_ATTEMPTS: u32 = 5;
    let statement = format!("GRANT USAGE ON SCHEMA public TO {test_role}");

    for attempt in 1..=MAX_ATTEMPTS {
        match sqlx::query(&statement).execute(pool).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < MAX_ATTEMPTS && is_retryable_public_schema_grant_error(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(attempt) * 25)).await;
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("grant_public_schema_usage_for_test_role should return within retry loop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_max_connections_defaults_to_ten() {
        assert_eq!(parse_database_max_connections(None), 10);
    }

    #[test]
    fn database_max_connections_uses_env_override() {
        assert_eq!(parse_database_max_connections(Some("24")), 24);
    }

    #[test]
    fn database_max_connections_rejects_zero_and_invalid_values() {
        assert_eq!(parse_database_max_connections(Some("0")), 10);
        assert_eq!(parse_database_max_connections(Some("not-a-number")), 10);
    }

    #[tokio::test]
    async fn pool_creation_and_migration() {
        let pool = shared_test_pool().await;

        // Verify we can query
        let row: (i32,) = sqlx::query_as("SELECT 1")
            .fetch_one(&pool)
            .await
            .expect("Failed to query");
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn set_tenant_context_in_transaction() {
        let pool = shared_test_pool().await;

        let tenant_id = uuid::Uuid::new_v4();

        // set_config with `true` (local) only persists within the same transaction
        let mut tx = pool.begin().await.expect("Failed to begin tx");

        set_tenant_context(&mut *tx, &tenant_id)
            .await
            .expect("Failed to set tenant context");

        let row: (String,) = sqlx::query_as("SELECT current_setting('rls.tenant_id', true)")
            .fetch_one(&mut *tx)
            .await
            .expect("Failed to query");
        assert_eq!(row.0, tenant_id.to_string());

        tx.rollback().await.expect("Failed to rollback");
    }
}
