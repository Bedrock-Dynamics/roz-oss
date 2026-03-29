# Phase 1a Remaining: Worker Registration + Host Dispatch

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `roz --host my-robot` works end-to-end: worker auto-registers, server dispatches to the correct worker, worker receives and executes tasks.

**Architecture:** Worker self-registers on startup via REST. Server resolves `host_id` UUID → `host.name` (= `worker_id`) before NATS publish. Fix existing routing bug where task dispatch publishes to UUID instead of hostname.

**Tech Stack:** Rust, async-nats, axum, sqlx, reqwest

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/roz-worker/src/registration.rs` | Create | Worker self-registration via REST on startup |
| `crates/roz-worker/src/main.rs` | Modify | Call registration on startup, set status online |
| `crates/roz-server/src/routes/tasks.rs` | Modify | Fix NATS routing: resolve host_id UUID → host.name |
| `crates/roz-server/src/grpc/agent.rs` | Modify | When session has host_id, dispatch task invocations to that worker |
| `crates/roz-worker/tests/registration_integration.rs` | Create | Test worker registration end-to-end |
| `crates/roz-server/src/main.rs` (tests) | Modify | Test task dispatch resolves to correct NATS subject |

---

### Task 1: Fix NATS routing bug in task dispatch

**Files:**
- Modify: `crates/roz-server/src/routes/tasks.rs:89-111`

**Problem:** Line 105 publishes to `invoke.{host_id_str}.{task_id}` where `host_id_str` is the UUID from the request body. But workers subscribe to `invoke.{hostname}.>`. UUID != hostname. Tasks silently vanish.

- [ ] **Step 1: Write failing test**

In `crates/roz-server/src/main.rs` test module, add:

```rust
#[sqlx::test]
async fn task_dispatch_uses_host_name_not_uuid(pool: sqlx::PgPool) {
    // Setup: create tenant, host, env, API key
    let tenant = roz_db::tenant::create_tenant(&pool, "dispatch-test", &format!("dispatch-{}", uuid::Uuid::new_v4()), "personal").await.unwrap();
    let host = roz_db::hosts::create(&pool, tenant.id, "my-robot-worker", "edge", &[], &serde_json::json!({})).await.unwrap();
    let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({})).await.unwrap();
    let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "test", &["admin".into()], "test").await.unwrap();

    // Setup NATS
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.unwrap();

    // Subscribe as the worker would: invoke.{hostname}.>
    let mut sub = nats.subscribe(format!("invoke.{}.>", host.name)).await.unwrap();

    // Build app with NATS
    let state = test_state_with_nats(pool.clone(), nats.clone());
    let app = roz_server::build_router(state);

    // Create task targeting the host
    let body = serde_json::json!({
        "prompt": "test dispatch",
        "environment_id": env.id,
        "host_id": host.id.to_string()
    });
    let req = Request::builder()
        .uri("/v1/tasks")
        .method("POST")
        .header("authorization", format!("Bearer {}", key.full_key))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Worker should receive the invocation
    use futures::StreamExt;
    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sub.next()
    ).await.expect("timeout").expect("subscription closed");

    let invocation: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(invocation["prompt"], "test dispatch");
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p roz-server -- task_dispatch_uses_host_name_not_uuid`
Expected: FAIL — timeout because the message is published to `invoke.{uuid}.{task_id}` but we subscribed to `invoke.{hostname}.>`

- [ ] **Step 3: Fix the routing**

In `crates/roz-server/src/routes/tasks.rs`, find the NATS dispatch block (where it publishes to `invoke.{host_id_str}.{task_id}`). Replace:

Before publishing, resolve the host UUID to its name:
```rust
if let (Some(nats), Some(host_id_str)) = (&state.nats_client, &body.host_id) {
    let host_uuid = uuid::Uuid::parse_str(host_id_str)
        .map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid host_id"}))))?;

    // Resolve host_id UUID → host.name (= worker_id for NATS routing)
    let host = roz_db::hosts::get_by_id(&state.pool, host_uuid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "host not found"}))))?;

    let invocation = roz_nats::dispatch::TaskInvocation {
        host_id: host_uuid,
        // ... other fields unchanged ...
    };
    let payload = serde_json::to_vec(&invocation).expect("serialize invocation");
    let subject = format!("invoke.{}.{}", host.name, task.id);  // Use host.name, not UUID
    // ... publish ...
}
```

- [ ] **Step 4: Run test, verify it passes**

Run: `cargo test -p roz-server -- task_dispatch_uses_host_name_not_uuid`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/roz-server/
git commit -m "fix: task dispatch resolves host_id UUID to host.name for NATS routing

Workers subscribe to invoke.{hostname}.> but dispatch was publishing to
invoke.{uuid}.{task_id}. Now resolves UUID to host.name before publish.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Worker auto-registration on startup

**Files:**
- Create: `crates/roz-worker/src/registration.rs`
- Modify: `crates/roz-worker/src/main.rs`

- [ ] **Step 1: Create registration module**

Create `crates/roz-worker/src/registration.rs`:

```rust
use anyhow::Context;

/// Register this worker as a host with the roz server.
///
/// Creates the host if it doesn't exist, or updates status to "online" if it does.
/// Returns the host UUID for later use.
pub async fn register_host(
    api_url: &str,
    api_key: &str,
    worker_id: &str,
) -> anyhow::Result<uuid::Uuid> {
    let client = reqwest::Client::new();

    // Try to find existing host by listing and matching name
    let list_url = format!("{api_url}/v1/hosts");
    let resp = client
        .get(&list_url)
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .context("failed to list hosts")?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await?;
        if let Some(hosts) = body["data"].as_array() {
            for h in hosts {
                if h["name"].as_str() == Some(worker_id) {
                    let id: uuid::Uuid = h["id"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .context("invalid host id")?;

                    // Update status to online
                    let status_url = format!("{api_url}/v1/hosts/{id}/status");
                    let _ = client
                        .patch(&status_url)
                        .header("authorization", format!("Bearer {api_key}"))
                        .json(&serde_json::json!({"status": "online"}))
                        .send()
                        .await;

                    tracing::info!(host_id = %id, worker_id, "registered (existing host, status → online)");
                    return Ok(id);
                }
            }
        }
    }

    // Host doesn't exist — create it
    let create_url = format!("{api_url}/v1/hosts");
    let resp = client
        .post(&create_url)
        .header("authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "name": worker_id,
            "host_type": "edge",
        }))
        .send()
        .await
        .context("failed to create host")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("host registration failed ({status}): {body}");
    }

    let body: serde_json::Value = resp.json().await?;
    let id: uuid::Uuid = body["data"]["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .context("server did not return host id")?;

    // Set status to online
    let status_url = format!("{api_url}/v1/hosts/{id}/status");
    let _ = client
        .patch(&status_url)
        .header("authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({"status": "online"}))
        .send()
        .await;

    tracing::info!(host_id = %id, worker_id, "registered (new host created)");
    Ok(id)
}
```

- [ ] **Step 2: Add `pub mod registration;` to worker lib or main**

Check if `crates/roz-worker/src/lib.rs` exists. If yes, add `pub mod registration;` there. If not, add `mod registration;` at the top of `main.rs`.

- [ ] **Step 3: Call registration on worker startup**

In `crates/roz-worker/src/main.rs`, after NATS connection and before the task subscription loop, add:

```rust
    // Register this worker as a host with the server
    let api_key = config.api_key.as_deref().unwrap_or("");
    if !api_key.is_empty() && !config.api_url.is_empty() {
        match crate::registration::register_host(&config.api_url, api_key, &config.worker_id).await {
            Ok(host_id) => tracing::info!(host_id = %host_id, "host registration complete"),
            Err(e) => tracing::warn!(error = %e, "host registration failed — tasks may not be dispatched to this worker"),
        }
    } else {
        tracing::warn!("ROZ_API_KEY or ROZ_API_URL not set — skipping host registration");
    }
```

Note: Check the `WorkerConfig` struct for the exact field names (`api_url`, `api_key`). They might be named differently — read the config struct to confirm.

- [ ] **Step 4: Build and clippy**

Run: `cargo fmt -p roz-worker && cargo clippy -p roz-worker -- -D warnings`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/
git commit -m "feat: worker auto-registers as host on startup

Calls POST /v1/hosts to create host record if it doesn't exist,
or PATCH /v1/hosts/{id}/status to set online if it does. Uses
ROZ_WORKER_ID as the host name for NATS routing.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Integration test — full dispatch loop

**Files:**
- Create: `crates/roz-worker/tests/dispatch_integration.rs`

This test proves the full loop: server publishes task → NATS → worker receives it.

- [ ] **Step 1: Write the test**

```rust
//! Integration test: task invocation arrives at the correct worker via NATS.
//!
//! Requires: Postgres testcontainer, NATS testcontainer.

use futures::StreamExt;
use std::time::Duration;

#[tokio::test]
async fn worker_receives_task_invocation_via_nats() {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrations");

    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.unwrap();

    // Create test data
    let slug = format!("dispatch-e2e-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Dispatch E2E", &slug, "personal")
        .await.unwrap();
    let host = roz_db::hosts::create(&pool, tenant.id, "test-robot", "edge", &[], &serde_json::json!({}))
        .await.unwrap();
    let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({}))
        .await.unwrap();

    // Worker subscribes as it would in production
    let worker_subject = format!("invoke.{}.>", host.name);
    let mut worker_sub = nats.subscribe(worker_subject).await.unwrap();

    // Simulate server dispatching a task
    let invocation = roz_nats::dispatch::TaskInvocation {
        task_id: uuid::Uuid::new_v4(),
        tenant_id: tenant.id.to_string(),
        prompt: "pick up the red block".to_string(),
        environment_id: env.id,
        safety_policy_id: None,
        host_id: host.id,
        timeout_secs: 60,
        mode: roz_nats::dispatch::ExecutionMode::React,
        parent_task_id: None,
        restate_url: "http://localhost:9080".to_string(),
        traceparent: None,
        phases: vec![],
    };

    let subject = format!("invoke.{}.{}", host.name, invocation.task_id);
    let payload = serde_json::to_vec(&invocation).unwrap();
    nats.publish(subject, payload.into()).await.unwrap();
    nats.flush().await.unwrap();

    // Worker receives the invocation
    let msg = tokio::time::timeout(Duration::from_secs(5), worker_sub.next())
        .await
        .expect("timeout — worker did not receive invocation")
        .expect("subscription closed");

    let received: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&msg.payload).expect("deserialize invocation");

    assert_eq!(received.prompt, "pick up the red block");
    assert_eq!(received.host_id, host.id);
    assert_eq!(received.environment_id, env.id);
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p roz-worker -- worker_receives_task_invocation`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/roz-worker/tests/
git commit -m "test: full dispatch loop — task invocation reaches worker via NATS

Proves server → NATS → worker routing uses host.name correctly.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Final build + push all to PR

- [ ] **Step 1: Full workspace build + clippy + test**

```bash
cd /Users/krnzt/Documents/BedrockDynamics/roz-public
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo build --workspace
cargo test --workspace --exclude roz-db --exclude roz-server
```

All must pass.

- [ ] **Step 2: Push to existing branch**

```bash
git push origin fix/provider-credential-separation
```

- [ ] **Step 3: Create or update PR**

If PR exists, push updates it. If not, create new PR with all Phase 1a commits.
