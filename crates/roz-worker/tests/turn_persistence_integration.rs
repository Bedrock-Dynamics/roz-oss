//! Worker-side DEBT-03 integration test.
//!
//! Verifies that `build_turn_flush(&config)` with a configured Postgres URL
//! returns a live emitter whose turns flow through `run_flush_task` and land
//! in `roz_session_turns` with correct `turn_index/role/content`.
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-worker --test turn_persistence_integration -- --test-threads=1
//! ```

use std::time::Duration;

use figment::{Figment, providers::Serialized};
use roz_agent::agent_loop::TurnEnvelope;
use roz_worker::config::WorkerConfig;
use roz_worker::turn_flush::build_turn_flush;
use serde_json::json;
use tokio::time::sleep;
use uuid::Uuid;

fn base_config(database_url: Option<String>) -> WorkerConfig {
    let mut vals = json!({
        "api_url": "http://localhost:8080",
        "nats_url": "nats://localhost:4222",
        "restate_url": "http://localhost:9080",
        "api_key": "roz_sk_test",
        "gateway_api_key": "paig_test",
    });
    if let Some(url) = database_url {
        vals["database_url"] = json!(url);
    }
    let figment = Figment::new().merge(Serialized::defaults(vals));
    WorkerConfig::from_figment(&figment).expect("config")
}

#[tokio::test]
async fn worker_flush_task_persists_turns() {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    // Bring up schema via a separate admin pool, then leak the guard.
    let admin = roz_db::create_pool(&url).await.expect("admin pool");
    roz_db::run_migrations(&admin).await.expect("migrate");

    let tenant_id = roz_db::tenant::create_tenant(&admin, "Test", &format!("ext-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant")
        .id;
    let env_id = roz_db::environments::create(&admin, tenant_id, "test-env", "simulation", &json!({}))
        .await
        .expect("env")
        .id;
    let session_id = roz_db::agent_sessions::create_session(&admin, tenant_id, env_id, "test-model")
        .await
        .expect("session")
        .id;

    let cfg = base_config(Some(url.clone()));
    let bundle = build_turn_flush(&cfg).await;
    let emitter = bundle.emitter.clone().expect("emitter when URL configured");

    // Emit a user + assistant turn, then drain.
    for (i, role) in [(0i32, "user"), (1i32, "assistant")] {
        emitter.emit(TurnEnvelope {
            session_id,
            tenant_id,
            turn_index: i,
            role,
            content: json!({ "i": i }),
            token_usage: None,
            kind: TurnEnvelope::KIND_TURN,
        });
    }
    sleep(Duration::from_millis(250)).await;
    bundle.drain().await;

    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT turn_index, role FROM roz_session_turns WHERE session_id = $1 ORDER BY turn_index")
            .bind(session_id)
            .fetch_all(&admin)
            .await
            .expect("select");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (0, "user".into()));
    assert_eq!(rows[1], (1, "assistant".into()));

    std::mem::forget(guard);
}
