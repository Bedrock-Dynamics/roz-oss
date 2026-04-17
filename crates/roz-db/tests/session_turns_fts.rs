//! Phase 17 MEM-01 integration tests for the FTS column + helper on
//! `roz_session_turns`.
//!
//! Covers:
//! - `content_tsv` extracts real lexemes (not raw JSON braces) from the
//!   `[{"text": "..."}]` message-part shape (Pitfall 2).
//! - `search_by_tsquery` returns ranked matches with non-empty snippets.
//! - Compaction-kind turns (`kind = 'compaction'`) are excluded from search
//!   (MEM-06 D-01 boundary).
//! - Cross-tenant FTS isolation (RLS via `current_setting('rls.tenant_id')`).
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test session_turns_fts -- --ignored --test-threads=1
//! ```

use roz_db::session_turns;
use roz_db::set_tenant_context;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn pg_pool_with_two_tenants() -> (PgPool, Uuid, Uuid) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);

    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", &format!("ext-a-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant a")
        .id;
    let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &format!("ext-b-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant b")
        .id;
    (pool, tenant_a, tenant_b)
}

/// Create an `roz_agent_sessions` row for `tenant_id` and return its id.
/// Uses the production helper so column-list drift cannot break the test.
async fn create_session(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    let env = roz_db::environments::create(pool, tenant_id, "test-env", "simulation", &json!({}))
        .await
        .expect("env");
    roz_db::agent_sessions::create_session(pool, tenant_id, env.id, "test-model")
        .await
        .expect("session")
        .id
}

#[tokio::test]
#[ignore = "requires docker"]
async fn tsvector_extracts_real_words_not_json_punctuation() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let session_id = create_session(&pool, tenant_a).await;

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    // Content shape matches agent message parts: [{"text": "..."}].
    let content = json!([{"text": "The calibration of the diff_drive encoder drifted at joint shoulder."}]);
    session_turns::insert_turn_with_kind(&mut *tx, session_id, 0, "assistant", &content, None, "turn")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Inspect the generated tsvector via ::text serialization.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let row: (String,) =
        sqlx::query_as("SELECT content_tsv::text FROM roz_session_turns WHERE session_id = $1 AND turn_index = 0")
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();

    let tsv = row.0;
    assert!(
        tsv.contains("calibr") || tsv.contains("calibration"),
        "tsv should contain a real lexeme: {tsv}"
    );
    assert!(
        tsv.contains("encoder") || tsv.contains("encod"),
        "tsv should contain 'encoder' lexeme: {tsv}"
    );
    // The to_tsvector output is a series of `'lexeme':positions` pairs separated
    // by spaces — it should never start with a JSON object brace.
    assert!(
        !tsv.starts_with('{') && !tsv.starts_with('['),
        "tsv must not look like raw JSON: {tsv}"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn search_by_tsquery_ranks_matches_and_snippets() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let session_id = create_session(&pool, tenant_a).await;

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_id,
        0,
        "assistant",
        &json!([{"text": "I calibrated the diff_drive encoder successfully and verified the calibration."}]),
        None,
        "turn",
    )
    .await
    .unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_id,
        1,
        "user",
        &json!([{"text": "Unrelated message about weather and clouds."}]),
        None,
        "turn",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let hits = session_turns::search_by_tsquery(&mut *tx, "calibration encoder", 10)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(!hits.is_empty(), "must get at least one ranked hit");
    let top = &hits[0];
    assert_eq!(top.turn_index, 0, "calibration turn must rank first");
    assert!(top.rank > 0.0, "rank must be positive for matched lexemes");
    assert!(!top.snippet.is_empty(), "snippet should be non-empty");
    // The unrelated weather row contains no overlap with the query — it
    // must not appear in the result set at all.
    assert!(
        hits.iter().all(|h| h.turn_index != 1),
        "unrelated turn should not appear: {hits:?}"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn compaction_turns_excluded_from_search() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let session_id = create_session(&pool, tenant_a).await;

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_id,
        0,
        "system",
        &json!([{"text": "calibration summary distilled from a prior context window"}]),
        None,
        "compaction",
    )
    .await
    .unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_id,
        1,
        "assistant",
        &json!([{"text": "normal calibration turn from the live session"}]),
        None,
        "turn",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let hits = session_turns::search_by_tsquery(&mut *tx, "calibration", 10)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(
        hits.iter().all(|h| h.turn_index != 0),
        "compaction turn must not surface in FTS: {hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.turn_index == 1),
        "the live 'turn' row must surface"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn fts_isolates_tenants() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;
    let session_a = create_session(&pool, tenant_a).await;
    let session_b = create_session(&pool, tenant_b).await;

    // Tenant A inserts a unique-phrase turn.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_a,
        0,
        "assistant",
        &json!([{"text": "tenant_a_secret_phrase appears only here"}]),
        None,
        "turn",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Tenant B inserts an unrelated turn so its session has data.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    session_turns::insert_turn_with_kind(
        &mut *tx,
        session_b,
        0,
        "assistant",
        &json!([{"text": "totally unrelated content for tenant b"}]),
        None,
        "turn",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Tenant B searches for tenant_a's unique phrase — must see nothing.
    // `search_by_tsquery` joins through `roz_agent_sessions` and filters
    // on `current_setting('rls.tenant_id')` so the scope is enforced even
    // as superuser (the SQL filter is defense-in-depth on top of RLS).
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let hits = session_turns::search_by_tsquery(&mut *tx, "tenant_a_secret_phrase", 10)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(
        hits.is_empty(),
        "tenant_b must not see tenant_a's turns via FTS: {hits:?}"
    );
}
