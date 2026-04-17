//! Integration coverage for `roz_agent::agent_loop::turn_emitter::run_flush_task`.
//!
//! These tests verify the write-behind DB flush task end-to-end against a
//! real Postgres (via `roz_test::pg_container()`):
//!
//! - `run_flush_task_persists_turns` — three in-order emits land as rows.
//! - `run_flush_task_drop_newest_preserves_earlier` — capacity-2 buffer +
//!   five emits persists exactly the first two (drop-newest overflow policy).
//! - `resume_seeds_turn_index` — flush task rewrites LOCAL indices via
//!   per-session `MAX(turn_index)+1` base offset.
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-agent --test turn_emitter_integration -- --test-threads=1
//! ```

use std::time::Duration;

use roz_agent::agent_loop::{TurnEmitter, TurnEnvelope, run_flush_task};
use serde_json::json;
use sqlx::PgPool;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn make_pool() -> PgPool {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);
    pool
}

async fn create_tenant(pool: &PgPool, slug: &str) -> Uuid {
    roz_db::tenant::create_tenant(pool, "Test", slug, "personal")
        .await
        .expect("tenant")
        .id
}

async fn create_session(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    let env = roz_db::environments::create(pool, tenant_id, "test-env", "simulation", &json!({}))
        .await
        .expect("env");
    roz_db::agent_sessions::create_session(pool, tenant_id, env.id, "test-model")
        .await
        .expect("session")
        .id
}

async fn drain_and_cancel(handle: tokio::task::JoinHandle<()>, cancel: &CancellationToken) {
    sleep(Duration::from_millis(250)).await;
    cancel.cancel();
    handle.await.expect("flush task join");
}

#[tokio::test]
async fn run_flush_task_persists_turns() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    let (emitter, rx) = TurnEmitter::new();
    let cancel = CancellationToken::new();
    let flush_pool = pool.clone();
    let flush_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_flush_task(rx, flush_pool, flush_cancel).await;
    });

    for i in 0..3i32 {
        emitter.emit(TurnEnvelope {
            session_id,
            tenant_id,
            turn_index: i,
            role: "user",
            content: json!({ "i": i }),
            token_usage: None,
            kind: TurnEnvelope::KIND_TURN,
        });
    }

    drain_and_cancel(handle, &cancel).await;

    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT turn_index, role FROM roz_session_turns WHERE session_id = $1 ORDER BY turn_index")
            .bind(session_id)
            .fetch_all(&pool)
            .await
            .expect("select");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], (0, "user".into()));
    assert_eq!(rows[1], (1, "user".into()));
    assert_eq!(rows[2], (2, "user".into()));
}

#[tokio::test]
async fn run_flush_task_drop_newest_preserves_earlier() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    // capacity=2, do NOT spawn flush task yet — indices 2..5 will be dropped.
    let (emitter, rx) = TurnEmitter::with_capacity(2);
    for i in 0..5i32 {
        emitter.emit(TurnEnvelope {
            session_id,
            tenant_id,
            turn_index: i,
            role: "user",
            content: json!({ "i": i }),
            token_usage: None,
            kind: TurnEnvelope::KIND_TURN,
        });
    }

    let cancel = CancellationToken::new();
    let flush_pool = pool.clone();
    let flush_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_flush_task(rx, flush_pool, flush_cancel).await;
    });
    drain_and_cancel(handle, &cancel).await;

    let rows: Vec<(i32,)> =
        sqlx::query_as("SELECT turn_index FROM roz_session_turns WHERE session_id = $1 ORDER BY turn_index")
            .bind(session_id)
            .fetch_all(&pool)
            .await
            .expect("select");
    let indices: Vec<i32> = rows.into_iter().map(|(i,)| i).collect();
    assert_eq!(indices, vec![0, 1], "drop-newest must preserve earliest two");
}

#[tokio::test]
async fn resume_seeds_turn_index() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    // Pre-insert turns 0,1,2 as if a prior run persisted them.
    for i in 0..3i32 {
        roz_db::session_turns::insert_turn(&pool, session_id, i, "user", &json!({ "i": i }), None)
            .await
            .expect("pre-insert");
    }

    // Flush task must rewrite LOCAL indices 0,1 → ABSOLUTE 3,4.
    let (emitter, rx) = TurnEmitter::new();
    let cancel = CancellationToken::new();
    let flush_pool = pool.clone();
    let flush_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_flush_task(rx, flush_pool, flush_cancel).await;
    });

    for i in 0..2i32 {
        emitter.emit(TurnEnvelope {
            session_id,
            tenant_id,
            turn_index: i,
            role: "assistant",
            content: json!({ "i": i }),
            token_usage: None,
            kind: TurnEnvelope::KIND_TURN,
        });
    }

    drain_and_cancel(handle, &cancel).await;

    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT turn_index, role FROM roz_session_turns WHERE session_id = $1 ORDER BY turn_index")
            .bind(session_id)
            .fetch_all(&pool)
            .await
            .expect("select");
    let indices: Vec<i32> = rows.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![0, 1, 2, 3, 4], "new turns must continue at MAX+1");
    assert_eq!(rows[3].1, "assistant");
    assert_eq!(rows[4].1, "assistant");
}
