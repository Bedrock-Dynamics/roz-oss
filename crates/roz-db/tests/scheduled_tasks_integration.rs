//! Phase 21 scheduled-task live-Postgres integration tests for `roz_db::scheduled_tasks`.
//!
//! Covers:
//! - create/get/list round-trip with NL + cron audit fields
//! - cross-tenant RLS isolation under a restricted role
//! - fire-marker + enable/disable bookkeeping without mutating audit fields
//!
//! Run with:
//!
//! ```bash
//! cargo test -p roz-db --test scheduled_tasks_integration -- --ignored --test-threads=1
//! ```

use chrono::{TimeZone, Utc};
use roz_core::schedule::CatchUpPolicy;
use roz_db::scheduled_tasks::{self, NewScheduledTask};
use roz_db::set_tenant_context;
use sqlx::PgPool;
use uuid::Uuid;

async fn pg_pool_with_two_tenants() -> (PgPool, Uuid, Uuid) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);

    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", &format!("sched-a-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant a")
        .id;
    let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &format!("sched-b-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant b")
        .id;
    (pool, tenant_a, tenant_b)
}

async fn create_restricted_role(pool: &PgPool) -> String {
    let role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));
    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
        .execute(pool)
        .await
        .expect("create role");
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}"))
        .execute(pool)
        .await
        .expect("grant schema");
    for table in ["roz_scheduled_tasks", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

fn task_template(prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "prompt": prompt,
        "environment_id": Uuid::nil(),
        "timeout_secs": 900,
        "phases": [],
    })
}

fn scheduled_task(name: &str) -> NewScheduledTask {
    NewScheduledTask {
        name: name.into(),
        nl_schedule: "every weekday at 9am Eastern".into(),
        parsed_cron: "0 0 9 * * Mon-Fri".into(),
        timezone: "America/New_York".into(),
        task_template: task_template("run diagnostics"),
        enabled: true,
        catch_up_policy: CatchUpPolicy::RunLatest,
        next_fire_at: Some(Utc.with_ymd_and_hms(2026, 4, 17, 13, 0, 0).unwrap()),
        last_fire_at: None,
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn scheduled_task_roundtrip_persists_audit_fields() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let created = {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        let row = scheduled_tasks::create(&mut *tx, scheduled_task("warehouse-morning"))
            .await
            .expect("create scheduled task");
        tx.commit().await.unwrap();
        row
    };

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let fetched = scheduled_tasks::get(&mut *tx, created.id)
        .await
        .expect("get")
        .expect("row exists");
    let listed = scheduled_tasks::list(&mut *tx, 50, 0).await.expect("list");
    tx.commit().await.unwrap();

    assert_eq!(fetched.tenant_id, tenant_a);
    assert_eq!(fetched.name, "warehouse-morning");
    assert_eq!(fetched.nl_schedule, "every weekday at 9am Eastern");
    assert_eq!(fetched.parsed_cron, "0 0 9 * * Mon-Fri");
    assert_eq!(fetched.timezone, "America/New_York");
    assert_eq!(fetched.task_template, task_template("run diagnostics"));
    assert!(fetched.enabled);
    assert_eq!(fetched.catch_up_policy, CatchUpPolicy::RunLatest.as_str());
    assert_eq!(
        fetched.next_fire_at,
        Some(Utc.with_ymd_and_hms(2026, 4, 17, 13, 0, 0).unwrap())
    );
    assert!(fetched.last_fire_at.is_none());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn scheduled_task_rls_blocks_cross_tenant_access() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;
    let created = {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        let row = scheduled_tasks::create(&mut *tx, scheduled_task("tenant-a-only"))
            .await
            .expect("create scheduled task");
        tx.commit().await.unwrap();
        row
    };

    let role = create_restricted_role(&pool).await;

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let fetched = scheduled_tasks::get(&mut *tx, created.id).await.unwrap();
    let listed = scheduled_tasks::list(&mut *tx, 50, 0).await.unwrap();
    let enabled = scheduled_tasks::list_enabled(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();

    assert!(fetched.is_none(), "tenant B must not see tenant A scheduled task");
    assert!(listed.is_empty(), "tenant B list must not leak tenant A rows");
    assert!(enabled.is_empty(), "tenant B enabled list must be empty");

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    assert!(scheduled_tasks::get(&mut *tx, created.id).await.unwrap().is_some());
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires docker"]
async fn scheduled_task_bookkeeping_updates_markers_without_mutating_audit_fields() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let created = {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        let row = scheduled_tasks::create(&mut *tx, scheduled_task("nightly-report"))
            .await
            .expect("create scheduled task");
        tx.commit().await.unwrap();
        row
    };

    let fired_at = Utc.with_ymd_and_hms(2026, 4, 17, 13, 0, 0).unwrap();
    let rescheduled_for = Utc.with_ymd_and_hms(2026, 4, 18, 13, 0, 0).unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let progressed = scheduled_tasks::record_fire_progress(&mut *tx, created.id, Some(fired_at), Some(rescheduled_for))
        .await
        .expect("record progress")
        .expect("row exists");
    let disabled = scheduled_tasks::set_enabled(&mut *tx, created.id, false, None)
        .await
        .expect("disable")
        .expect("row exists");
    let removed = scheduled_tasks::delete(&mut *tx, created.id).await.expect("delete");
    let after_delete = scheduled_tasks::get(&mut *tx, created.id)
        .await
        .expect("get after delete");
    tx.commit().await.unwrap();

    assert_eq!(progressed.last_fire_at, Some(fired_at));
    assert_eq!(progressed.next_fire_at, Some(rescheduled_for));
    assert_eq!(progressed.nl_schedule, "every weekday at 9am Eastern");
    assert_eq!(progressed.parsed_cron, "0 0 9 * * Mon-Fri");
    assert_eq!(disabled.nl_schedule, "every weekday at 9am Eastern");
    assert_eq!(disabled.parsed_cron, "0 0 9 * * Mon-Fri");
    assert_eq!(disabled.timezone, "America/New_York");
    assert_eq!(disabled.catch_up_policy, CatchUpPolicy::RunLatest.as_str());
    assert!(!disabled.enabled);
    assert!(disabled.next_fire_at.is_none());
    assert_eq!(removed, 1);
    assert!(after_delete.is_none());
}
