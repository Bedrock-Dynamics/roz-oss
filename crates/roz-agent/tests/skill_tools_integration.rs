//! Phase 18 PLAN-10 Task 3: end-to-end dispatch tests for the four `skill_*`
//! tools against a live Postgres + tempdir LocalFileSystem object store.
//!
//! Coverage (per 18-10-PLAN must-haves):
//! - skills_list returns recent entries for the calling tenant
//! - skill_view loads body + frontmatter and emits `SessionEvent::SkillLoaded`
//!   when a session runtime injects an `EventEmitter`
//! - skill_read_file rejects `..` traversal BEFORE touching the object_store
//! - skill_manage refuses without `can_write_skills` (no DB row written)
//! - skill_manage create succeeds with permission, persists the DB row, and
//!   emits `SessionEvent::SkillCrystallized`
//! - mid-session skill writes are immediately visible through `skills_list`
//!   and `skill_view`, but the frozen `skills_context` prompt snapshot does
//!   not change until a new session/runtime is constructed
//!
//! ```bash
//! cargo test -p roz-agent --test skill_tools_integration -- --ignored --test-threads=1
//! ```

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt as _, PutPayload};
use roz_agent::dispatch::skill_tools::{
    SkillManageInput, SkillManageTool, SkillReadFileInput, SkillReadFileTool, SkillViewInput, SkillViewTool,
    SkillsListInput, SkillsListTool,
};
use roz_agent::dispatch::{Extensions, ToolContext, TypedToolExecutor};
use roz_agent::prompt_assembler::ToolSchema;
use roz_agent::session_runtime::{EventEmitter, SessionConfig, SessionRuntime, TurnInput};
use roz_core::auth::Permissions;
use roz_core::session::control::{CognitionMode, SessionMode};
use roz_core::session::event::SessionEvent;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn pg_pool_with_tenant() -> (PgPool, Uuid) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);

    let tenant_id = roz_db::tenant::create_tenant(&pool, "Skill Test", &format!("ext-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant")
        .id;
    (pool, tenant_id)
}

fn ctx_with_extensions(
    tenant_id: Uuid,
    pool: PgPool,
    object_store: Option<Arc<dyn ObjectStore>>,
    perms: Permissions,
    event_emitter: Option<EventEmitter>,
) -> ToolContext {
    let mut ext = Extensions::new();
    ext.insert(pool);
    if let Some(store) = object_store {
        ext.insert(store);
    }
    ext.insert(perms);
    if let Some(emitter) = event_emitter {
        ext.insert(emitter);
    }
    ToolContext {
        task_id: "task-skill-test".into(),
        tenant_id: tenant_id.to_string(),
        call_id: String::new(),
        extensions: ext,
    }
}

fn fixture_frontmatter(name: &str, version: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "version": version,
    })
}

async fn seed_skill(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    version: &str,
    body_md: &str,
) -> roz_db::skills::SkillRow {
    let mut tx = pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &tenant_id).await.unwrap();
    let row = roz_db::skills::insert_skill(
        &mut *tx,
        name,
        version,
        body_md,
        &fixture_frontmatter(name, version, "fixture skill"),
        "local",
        "user:test",
    )
    .await
    .expect("seed insert");
    tx.commit().await.unwrap();
    row
}

async fn list_recent_skills(pool: &PgPool, tenant_id: Uuid, limit: i64) -> Vec<roz_db::skills::SkillSummary> {
    let mut tx = pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &tenant_id).await.unwrap();
    let rows = roz_db::skills::list_recent(&mut *tx, limit)
        .await
        .expect("list recent skills");
    tx.commit().await.unwrap();
    rows
}

fn test_session_runtime(tenant_id: Uuid) -> SessionRuntime {
    SessionRuntime::new(&SessionConfig {
        session_id: format!("sess-skill-test-{}", Uuid::new_v4()),
        tenant_id: tenant_id.to_string(),
        mode: SessionMode::Server,
        cognition_mode: CognitionMode::React,
        constitution_text: "Test constitution".into(),
        blueprint_toml: String::new(),
        model_name: Some("claude-sonnet-4-6".into()),
        permissions: vec![],
        tool_schemas: vec![ToolSchema {
            name: "skill_view".into(),
            description: "load skill body".into(),
            parameters_json: "{}".into(),
        }],
        project_context: vec![],
        initial_history: vec![],
    })
}

fn skills_context_block(runtime: &mut SessionRuntime) -> String {
    runtime
        .begin_turn(
            &TurnInput {
                user_message: "show skills".into(),
                cognition_mode: CognitionMode::React,
                custom_context: vec![],
                volatile_blocks: vec![],
            },
            Vec::new(),
        )
        .expect("begin turn for skills context")
        .system_blocks[2]
        .content
        .clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires docker"]
async fn skills_list_returns_recent_per_tenant() {
    let (pool, tenant_id) = pg_pool_with_tenant().await;

    seed_skill(&pool, tenant_id, "alpha-skill", "0.1.0", "# alpha").await;
    seed_skill(&pool, tenant_id, "beta-skill", "0.1.0", "# beta").await;

    let tool = SkillsListTool;
    let ctx = ctx_with_extensions(tenant_id, pool, None, Permissions::default(), None);
    let out = tool
        .execute(SkillsListInput { limit: Some(50) }, &ctx)
        .await
        .expect("execute");
    assert!(!out.is_error(), "skills_list should succeed: {:?}", out.error);
    let names: Vec<&str> = out
        .output
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get("name").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    assert!(names.contains(&"alpha-skill"), "missing alpha-skill in {names:?}");
    assert!(names.contains(&"beta-skill"), "missing beta-skill in {names:?}");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn skill_view_loads_body_and_emits_event() {
    let (pool, tenant_id) = pg_pool_with_tenant().await;
    let body = "# Test Skill\n\nbody here";
    seed_skill(&pool, tenant_id, "view-skill", "0.1.0", body).await;

    let tool = SkillViewTool;
    let emitter = EventEmitter::new(8);
    let mut event_rx = emitter.subscribe();
    let ctx = ctx_with_extensions(tenant_id, pool, None, Permissions::default(), Some(emitter.clone()));
    let out = tool
        .execute(
            SkillViewInput {
                name: "view-skill".into(),
                version: None,
            },
            &ctx,
        )
        .await
        .expect("execute");
    assert!(!out.is_error(), "skill_view should succeed: {:?}", out.error);
    let body_out = out
        .output
        .get("body_md")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(body_out, body, "body_md should match inserted body");
    let name_out = out
        .output
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(name_out, "view-skill");
    let envelope = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("skill_view should emit an event")
        .expect("event receiver should stay open");
    assert!(matches!(
        envelope.event,
        SessionEvent::SkillLoaded { ref name, ref version }
            if name == "view-skill" && version == "0.1.0"
    ));
}

#[tokio::test]
#[ignore = "requires docker"]
async fn skill_read_file_blocks_traversal_live() {
    // Prepare an object_store with one entry at the canonical bundle prefix;
    // dispatch with `path: "../escape"` MUST reject before the store is hit.
    let (pool, tenant_id) = pg_pool_with_tenant().await;
    let _row = seed_skill(&pool, tenant_id, "trav-skill", "0.1.0", "# body").await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(tmp.path()).expect("LocalFS"));
    let canonical = ObjPath::from(format!("{tenant_id}/trav-skill/0.1.0/scripts/hello.sh"));
    store
        .put(&canonical, PutPayload::from_bytes(Bytes::from_static(b"echo hi\n")))
        .await
        .expect("seed bundled file");

    let tool = SkillReadFileTool;
    let ctx = ctx_with_extensions(tenant_id, pool, Some(store), Permissions::default(), None);
    let out = tool
        .execute(
            SkillReadFileInput {
                name: "trav-skill".into(),
                version: None,
                path: "../escape".into(),
            },
            &ctx,
        )
        .await
        .expect("execute");
    assert!(out.is_error(), "traversal must reject");
    let msg = out.error.as_deref().unwrap_or_default();
    assert!(
        msg.contains("traversal") || msg.contains("path"),
        "error must mention traversal/path: {msg:?}"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn skill_manage_refuses_without_permission_live() {
    let (pool, tenant_id) = pg_pool_with_tenant().await;

    let tool = SkillManageTool;
    let ctx = ctx_with_extensions(tenant_id, pool.clone(), None, Permissions::default(), None);
    let out = tool
        .execute(
            SkillManageInput {
                skill_md: "---\nname: refused-skill\ndescription: fixture\nversion: 0.1.0\n---\nbody".into(),
            },
            &ctx,
        )
        .await
        .expect("execute");
    assert!(
        out.is_error(),
        "default Permissions has can_write_skills=false; must refuse"
    );
    let msg = out.error.as_deref().unwrap_or_default();
    assert!(msg.contains("can_write_skills"), "error must name gate: {msg:?}");

    // Live verification: NO row in roz_skills.
    let mut tx = pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &tenant_id).await.unwrap();
    let row = roz_db::skills::get_by_name_version(&mut *tx, "refused-skill", "0.1.0")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(row.is_none(), "no DB row should be persisted on permission refusal");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn skill_manage_create_succeeds_with_permission_emits_crystallized() {
    let (pool, tenant_id) = pg_pool_with_tenant().await;
    let perms = Permissions {
        can_write_memory: false,
        can_write_skills: true,
        can_manage_mcp_servers: false,
    };
    let tool = SkillManageTool;
    let emitter = EventEmitter::new(8);
    let mut event_rx = emitter.subscribe();
    let ctx = ctx_with_extensions(tenant_id, pool.clone(), None, perms, Some(emitter.clone()));
    let out = tool
        .execute(
            SkillManageInput {
                skill_md: "---\nname: crystal-skill\ndescription: fixture\nversion: 0.1.0\n---\nbody".into(),
            },
            &ctx,
        )
        .await
        .expect("execute");
    assert!(!out.is_error(), "skill_manage should succeed: {:?}", out.error);
    let name_out = out
        .output
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(name_out, "crystal-skill");

    // Row must be live in DB.
    let mut tx = pool.begin().await.unwrap();
    roz_db::set_tenant_context(&mut *tx, &tenant_id).await.unwrap();
    let row = roz_db::skills::get_by_name_version(&mut *tx, "crystal-skill", "0.1.0")
        .await
        .unwrap()
        .expect("row");
    tx.commit().await.unwrap();
    assert_eq!(row.name, "crystal-skill");
    assert_eq!(row.version, "0.1.0");
    assert_eq!(row.body_md, "body");
    let envelope = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("skill_manage should emit an event")
        .expect("event receiver should stay open");
    assert!(matches!(
        envelope.event,
        SessionEvent::SkillCrystallized {
            ref name,
            ref version,
            ref source,
        } if name == "crystal-skill" && version == "0.1.0" && source == "local"
    ));
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mid_session_skill_writes_are_live_but_frozen_prompt_snapshot_stays_stable() {
    let (pool, tenant_id) = pg_pool_with_tenant().await;
    seed_skill(
        &pool,
        tenant_id,
        "frozen-skill",
        "0.1.0",
        "# Frozen Skill\n\noriginal body",
    )
    .await;

    let initial_snapshot = list_recent_skills(&pool, tenant_id, 20).await;
    let mut runtime = test_session_runtime(tenant_id);
    runtime.set_skill_snapshot(initial_snapshot);
    let frozen_before = skills_context_block(&mut runtime);
    assert!(
        frozen_before.contains("frozen-skill v0.1.0"),
        "session-start prompt must include the initial skill snapshot: {frozen_before}"
    );
    assert!(
        !frozen_before.contains("live-skill v0.2.0"),
        "new skill must not appear before it is created: {frozen_before}"
    );

    let perms = Permissions {
        can_write_memory: false,
        can_write_skills: true,
        can_manage_mcp_servers: false,
    };
    let manage_ctx = ctx_with_extensions(tenant_id, pool.clone(), None, perms, None);
    let manage_tool = SkillManageTool;
    let create_out = manage_tool
        .execute(
            SkillManageInput {
                skill_md: "---\nname: live-skill\ndescription: created mid-session\nversion: 0.2.0\n---\n# Live Skill\n\nnew body".into(),
            },
            &manage_ctx,
        )
        .await
        .expect("execute skill_manage");
    assert!(
        !create_out.is_error(),
        "skill_manage should succeed mid-session: {:?}",
        create_out.error
    );

    let list_ctx = ctx_with_extensions(tenant_id, pool.clone(), None, Permissions::default(), None);
    let list_tool = SkillsListTool;
    let list_out = list_tool
        .execute(SkillsListInput { limit: Some(20) }, &list_ctx)
        .await
        .expect("execute skills_list");
    assert!(!list_out.is_error(), "skills_list should succeed: {:?}", list_out.error);
    let listed_live_skill = list_out.output.as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row.get("name").and_then(serde_json::Value::as_str) == Some("live-skill")
                && row.get("version").and_then(serde_json::Value::as_str) == Some("0.2.0")
        })
    });
    assert!(
        listed_live_skill,
        "skills_list must expose the live mid-session inventory: {}",
        list_out.output
    );

    let view_ctx = ctx_with_extensions(tenant_id, pool.clone(), None, Permissions::default(), None);
    let view_tool = SkillViewTool;
    let view_out = view_tool
        .execute(
            SkillViewInput {
                name: "live-skill".into(),
                version: Some("0.2.0".into()),
            },
            &view_ctx,
        )
        .await
        .expect("execute skill_view");
    assert!(!view_out.is_error(), "skill_view should succeed: {:?}", view_out.error);
    assert_eq!(
        view_out.output.get("body_md").and_then(serde_json::Value::as_str),
        Some("# Live Skill\n\nnew body"),
        "skill_view must load the live body/version immediately"
    );

    let frozen_after = skills_context_block(&mut runtime);
    assert_eq!(
        frozen_after, frozen_before,
        "existing runtime prompt snapshot must remain frozen for the life of the session"
    );
    assert!(
        !frozen_after.contains("live-skill v0.2.0"),
        "mid-session prompt snapshot must not pick up live skill writes: {frozen_after}"
    );

    let refreshed_snapshot = list_recent_skills(&pool, tenant_id, 20).await;
    let mut refreshed_runtime = test_session_runtime(tenant_id);
    refreshed_runtime.set_skill_snapshot(refreshed_snapshot);
    let refreshed_block = skills_context_block(&mut refreshed_runtime);
    assert!(
        refreshed_block.contains("live-skill v0.2.0"),
        "a new session/runtime must pick up the refreshed skill snapshot: {refreshed_block}"
    );
}
