# Phase 1a: Host-Targeted Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users can target a specific robot host when starting a session via CLI (`roz --host my-robot`), and trigger e-stop via REST.

**Architecture:** Add `--host` flag to CLI, pass `host_id` through gRPC `StartSession`, add `agent_placement` proto field, add estop REST endpoint, wire the existing dead-code estop listener in the worker.

**Tech Stack:** Rust, tonic/prost (gRPC + protobuf), clap (CLI), axum (REST), async-nats

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `proto/roz/v1/agent.proto` | Modify | Add `AgentPlacement` enum + field 8 to `StartSession` |
| `crates/roz-cli/src/cli.rs` | Modify | Add `--host`, `--cloud`, `--edge` flags to `GlobalOpts` |
| `crates/roz-cli/src/main.rs` | Modify | Pass `host` flag to interactive/non-interactive commands |
| `crates/roz-cli/src/tui/providers/cloud.rs` | Modify | Set `host_id` in `StartSession` from config |
| `crates/roz-cli/src/tui/provider.rs` | Modify | Add `host` field to `ProviderConfig` |
| `crates/roz-cli/src/commands/interactive.rs` | Modify | Pass `host` to `ProviderConfig` |
| `crates/roz-cli/src/commands/non_interactive.rs` | Modify | Pass `host` to `ProviderConfig` |
| `crates/roz-cli/src/commands/estop.rs` | Create | `roz estop <host>` command via REST |
| `crates/roz-server/src/routes/hosts.rs` | Modify | Add `POST /v1/hosts/{id}/estop` endpoint |
| `crates/roz-nats/src/subjects.rs` | Modify | Add `estop(worker_id)` builder |
| `crates/roz-worker/src/main.rs` | Modify | Wire `spawn_estop_listener()` |
| `crates/roz-server/src/grpc/agent.rs` | Modify | Store `host_id` in Session struct |

---

### Task 1: Add `AgentPlacement` enum to proto

**Files:**
- Modify: `proto/roz/v1/agent.proto:35-43`

- [ ] **Step 1: Add enum and field to proto**

In `proto/roz/v1/agent.proto`, add after the `StartSession` message (after line 43):

```protobuf
enum AgentPlacement {
  AGENT_PLACEMENT_AUTO = 0;
  AGENT_PLACEMENT_CLOUD = 1;
  AGENT_PLACEMENT_EDGE = 2;
}
```

And add field 8 to `StartSession`:

```protobuf
message StartSession {
  string environment_id = 1;
  optional string host_id = 2;
  optional string model = 3;
  repeated ToolSchema tools = 4;
  repeated ConversationMessage history = 5;
  repeated string project_context = 6;
  optional uint32 max_context_tokens = 7;
  optional AgentPlacement agent_placement = 8;
}
```

- [ ] **Step 2: Build to regenerate proto code**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-server -p roz-cli 2>&1 | tail -5`
Expected: Clean build (proto codegen runs via `build.rs`)

- [ ] **Step 3: Commit**

```bash
git add proto/roz/v1/agent.proto
git commit -m "proto: add AgentPlacement enum to StartSession

Field 8 on StartSession. Auto/Cloud/Edge placement for future
edge agent relay support. Phase 1a only uses Auto (cloud default).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Add `--host`, `--cloud`, `--edge` CLI flags

**Files:**
- Modify: `crates/roz-cli/src/cli.rs:20-64`

- [ ] **Step 1: Add flags to `GlobalOpts`**

In `crates/roz-cli/src/cli.rs`, add three new fields to the `GlobalOpts` struct:

```rust
    /// Target a specific robot host by name
    #[arg(long)]
    pub host: Option<String>,

    /// Force cloud agent (server-side reasoning)
    #[arg(long, conflicts_with = "edge")]
    pub cloud: bool,

    /// Force edge agent (robot-side reasoning)
    #[arg(long, conflicts_with = "cloud")]
    pub edge: bool,
```

- [ ] **Step 2: Add `estop` subcommand to `Commands` enum**

In the same file, add to the `Commands` enum:

```rust
    /// Emergency stop a robot host
    #[command(name = "estop")]
    Estop {
        /// Host name or ID to e-stop
        host: String,
    },
```

- [ ] **Step 3: Build to verify**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-cli 2>&1 | tail -5`
Expected: Clean build

- [ ] **Step 4: Commit**

```bash
git add crates/roz-cli/src/cli.rs
git commit -m "cli: add --host, --cloud, --edge flags and estop subcommand

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Add `host` field to `ProviderConfig` and pass through CLI

**Files:**
- Modify: `crates/roz-cli/src/tui/provider.rs:84-91`
- Modify: `crates/roz-cli/src/main.rs`
- Modify: `crates/roz-cli/src/commands/interactive.rs`
- Modify: `crates/roz-cli/src/commands/non_interactive.rs`

- [ ] **Step 1: Add `host` field to `ProviderConfig`**

In `crates/roz-cli/src/tui/provider.rs`, add to the `ProviderConfig` struct:

```rust
pub struct ProviderConfig {
    pub provider: Provider,
    pub model: String,
    pub api_key: Option<String>,
    pub api_url: String,
    pub host: Option<String>,
}
```

- [ ] **Step 2: Update all `ProviderConfig` construction sites**

In `detect()` and `for_provider_and_model()`, add `host: None` to every `Self { ... }` return. There are ~7 construction sites in `detect()` and 1 in `for_provider_and_model()`. Search for `Self {` in the file and add `host: None,` to each.

- [ ] **Step 3: Pass `--host` from `main.rs` to commands**

In `crates/roz-cli/src/main.rs`, the interactive and non-interactive calls need the host flag. Update the calls:

```rust
// Interactive mode
commands::interactive::execute(&config, model_flag, cli.global.host.as_deref(), resume, resume_session).await?;

// Non-interactive mode
commands::non_interactive::execute(&config, model_flag, cli.global.host.as_deref(), task).await?;
```

- [ ] **Step 4: Update `interactive::execute()` signature and set host on config**

In `crates/roz-cli/src/commands/interactive.rs`, update the function signature:

```rust
pub async fn execute(
    config: &CliConfig,
    model_flag: Option<&str>,
    host_flag: Option<&str>,
    resume: bool,
    resume_session: Option<&str>,
) -> anyhow::Result<()> {
```

After `ProviderConfig::detect()`, set the host:

```rust
    let mut provider_config = ProviderConfig::detect(
        model_flag,
        config.access_token.as_deref(),
        roz_toml.model_ref.as_deref(),
    );
    provider_config.host = host_flag.map(String::from);
```

- [ ] **Step 5: Update `non_interactive::execute()` signature**

In `crates/roz-cli/src/commands/non_interactive.rs`, update similarly:

```rust
pub async fn execute(config: &CliConfig, model_flag: Option<&str>, host_flag: Option<&str>, task: &str) -> anyhow::Result<()> {
```

Set host on provider_config after detect:
```rust
    let mut provider_config = ProviderConfig::detect(model_flag, config.access_token.as_deref(), roz_toml.as_deref());
    provider_config.host = host_flag.map(String::from);
```

- [ ] **Step 6: Handle estop subcommand in main.rs**

In `main.rs`, add the match arm for the estop command:

```rust
Some(Commands::Estop { host }) => {
    commands::estop::execute(&config, &host).await?;
}
```

- [ ] **Step 7: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-cli && cargo clippy -p roz-cli -- -D warnings 2>&1 | tail -5`
Expected: May fail on missing `estop` module — that's OK, created in Task 5

- [ ] **Step 8: Commit**

```bash
git add crates/roz-cli/src/tui/provider.rs crates/roz-cli/src/main.rs crates/roz-cli/src/commands/interactive.rs crates/roz-cli/src/commands/non_interactive.rs
git commit -m "cli: pass --host flag through to ProviderConfig

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Pass `host_id` in gRPC `StartSession`

**Files:**
- Modify: `crates/roz-cli/src/tui/providers/cloud.rs:43-48`

- [ ] **Step 1: Set `host_id` from config**

In `crates/roz-cli/src/tui/providers/cloud.rs`, update the `StartSession` construction (around line 43-48):

```rust
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: String::new(),
                host_id: config.host.clone(),
                model: Some(config.model.clone()),
                ..Default::default()
            })),
        })
        .await?;
```

Note: `host_id` is `optional string` in the proto, which generates `Option<String>` in Rust. `config.host` is already `Option<String>`.

- [ ] **Step 2: Build**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-cli 2>&1 | tail -5`
Expected: Clean

- [ ] **Step 3: Commit**

```bash
git add crates/roz-cli/src/tui/providers/cloud.rs
git commit -m "cli: pass host_id in gRPC StartSession

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Add `roz estop` command (REST-based)

**Files:**
- Create: `crates/roz-cli/src/commands/estop.rs`
- Modify: `crates/roz-cli/src/commands/mod.rs`

- [ ] **Step 1: Create estop command module**

Create `crates/roz-cli/src/commands/estop.rs`:

```rust
use crate::config::CliConfig;

/// Trigger emergency stop on a robot host via REST API.
pub async fn execute(config: &CliConfig, host: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;

    // Resolve host name to UUID if needed
    let host_id = resolve_host_id(&client, &config.api_url, host).await?;

    // POST /v1/hosts/{id}/estop
    let url = format!("{}/v1/hosts/{}/estop", config.api_url, host_id);
    let resp = client.post(&url).send().await?;

    if resp.status().is_success() {
        eprintln!("E-STOP sent to {host}");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("E-STOP failed ({status}): {body}");
    }

    Ok(())
}

/// Resolve a host name to its UUID via the REST API.
/// If the input is already a UUID, return it as-is.
async fn resolve_host_id(client: &reqwest::Client, api_url: &str, host: &str) -> anyhow::Result<String> {
    // If it parses as a UUID, use it directly
    if uuid::Uuid::parse_str(host).is_ok() {
        return Ok(host.to_string());
    }

    // Otherwise, list hosts and find by name
    let url = format!("{api_url}/v1/hosts");
    let resp = client.get(&url).send().await?;
    let body: serde_json::Value = resp.json().await?;

    let hosts = body["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected response format"))?;

    for h in hosts {
        if h["name"].as_str() == Some(host) {
            if let Some(id) = h["id"].as_str() {
                return Ok(id.to_string());
            }
        }
    }

    anyhow::bail!("host '{host}' not found. Run `roz host list` to see available hosts.")
}
```

- [ ] **Step 2: Add `estop` module to `commands/mod.rs`**

In `crates/roz-cli/src/commands/mod.rs`, add:

```rust
pub mod estop;
```

- [ ] **Step 3: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-cli && cargo clippy -p roz-cli -- -D warnings 2>&1 | tail -5`
Expected: Clean (or fail on missing server endpoint — that's OK, added in Task 6)

- [ ] **Step 4: Commit**

```bash
git add crates/roz-cli/src/commands/estop.rs crates/roz-cli/src/commands/mod.rs
git commit -m "cli: add roz estop command via REST

Resolves host name to UUID, calls POST /v1/hosts/{id}/estop.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Add `POST /v1/hosts/{id}/estop` server endpoint

**Files:**
- Modify: `crates/roz-server/src/routes/hosts.rs`
- Modify: `crates/roz-server/src/lib.rs` (add route)
- Modify: `crates/roz-nats/src/subjects.rs` (add estop subject builder)

- [ ] **Step 1: Add estop subject builder to roz-nats**

In `crates/roz-nats/src/subjects.rs`, add a new method to the `Subjects` impl:

```rust
    /// E-stop subject for a worker: `safety.estop.{worker_id}`
    pub fn estop(worker_id: &str) -> String {
        format!("safety.estop.{worker_id}")
    }
```

- [ ] **Step 2: Add estop route handler to hosts.rs**

In `crates/roz-server/src/routes/hosts.rs`, add the handler:

```rust
/// Trigger emergency stop on a host. Publishes to NATS `safety.estop.{host_name}`.
pub async fn estop(
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    // Look up the host to get its name (used as worker_id in NATS)
    let host = crate::routes::hosts::get_host_row(&state.pool, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "host not found"}))))?;

    // Publish e-stop to NATS
    if let Some(nats) = &state.nats_client {
        let subject = roz_nats::Subjects::estop(&host.name);
        nats.publish(subject, bytes::Bytes::from_static(b"{}"))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, host_id = %id, "failed to publish e-stop");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "failed to publish e-stop"})))
            })?;
        tracing::warn!(host_id = %id, host_name = %host.name, "E-STOP published");
    } else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "NATS not connected — cannot send e-stop"}))));
    }

    Ok((StatusCode::OK, Json(json!({"status": "estop_sent", "host_id": id, "host_name": host.name}))))
}
```

Note: this uses `host.name` as the `worker_id` for NATS subjects. This works because the worker registers with its `worker_id` as the host name. If a helper `get_host_row` doesn't exist, read the existing `get` handler in hosts.rs and extract/reuse the DB query.

- [ ] **Step 3: Add route to `lib.rs`**

In `crates/roz-server/src/lib.rs`, in the `build_router()` function, add after the existing host routes:

```rust
        .route("/v1/hosts/{id}/estop", post(routes::hosts::estop))
```

- [ ] **Step 4: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt --all && cargo clippy -p roz-server -p roz-nats -- -D warnings 2>&1 | tail -10`
Expected: Clean

- [ ] **Step 5: Commit**

```bash
git add crates/roz-server/src/routes/hosts.rs crates/roz-server/src/lib.rs crates/roz-nats/src/subjects.rs
git commit -m "server: add POST /v1/hosts/{id}/estop endpoint

Publishes safety.estop.{worker_id} to NATS. Adds estop subject
builder to roz-nats.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Wire `spawn_estop_listener()` in worker

**Files:**
- Modify: `crates/roz-worker/src/main.rs:160-185`

- [ ] **Step 1: Wire estop listener after NATS connection**

In `crates/roz-worker/src/main.rs`, after the heartbeat spawn (around line 177) and before the task subscription (around line 180), add:

```rust
    // Subscribe to e-stop events and spawn listener
    let estop_sub = crate::estop::subscribe_estop(&nats, &config.worker_id).await?;
    let estop_rx = crate::estop::spawn_estop_listener(estop_sub);
    tracing::info!(worker_id = %config.worker_id, "e-stop listener active");
```

- [ ] **Step 2: Check estop flag in the task execution loop**

In the task processing loop (after receiving a message from the NATS subscription), add an estop check before executing:

```rust
    while let Some(msg) = sub.next().await {
        // Check e-stop before processing
        if *estop_rx.borrow() {
            tracing::error!("E-STOP active — rejecting task invocation");
            continue;
        }
        // ... existing task processing code ...
    }
```

- [ ] **Step 3: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-worker && cargo clippy -p roz-worker -- -D warnings 2>&1 | tail -5`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/roz-worker/src/main.rs
git commit -m "worker: wire estop listener — was defined but never called

Subscribes to safety.estop.{worker_id} on startup. Rejects new task
invocations when e-stop is active.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Store `host_id` in server Session struct

**Files:**
- Modify: `crates/roz-server/src/grpc/agent.rs:1295` (Session struct construction)

- [ ] **Step 1: Add `host_id` to Session struct**

Find the `Session` struct definition in `agent.rs` and add:

```rust
    pub host_id: Option<String>,
```

- [ ] **Step 2: Set `host_id` from `StartSession` in `handle_start`**

In the `handle_start` function, where the Session is constructed (around line 1295), add:

```rust
    *session = Some(Session {
        id: session_id,
        tenant_id,
        environment_id: env_id,
        host_id: start.host_id,
        // ... rest of existing fields ...
    });
```

- [ ] **Step 3: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-server && cargo clippy -p roz-server -- -D warnings 2>&1 | tail -5`
Expected: Clean

- [ ] **Step 4: Commit**

```bash
git add crates/roz-server/src/grpc/agent.rs
git commit -m "server: store host_id in gRPC Session struct

Captures host_id from StartSession for future task dispatch
to specific robots.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: Full build + test + PR

**Files:** None (integration task)

- [ ] **Step 1: Full workspace build**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build --workspace 2>&1 | tail -5`
Expected: Clean

- [ ] **Step 2: Full clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo clippy --workspace -- -D warnings 2>&1 | tail -5`
Expected: Clean

- [ ] **Step 3: Full test suite**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test --workspace --exclude roz-db --exclude roz-server 2>&1 | tail -10`
Expected: All pass (DB/server tests excluded — need Postgres)

- [ ] **Step 4: Push and create PR**

```bash
git push origin fix/provider-credential-separation
gh pr create --title "feat: Phase 1a — host-targeted sessions + estop" --body "## Summary
- AgentPlacement enum added to proto (field 8)
- --host, --cloud, --edge CLI flags
- host_id passed through gRPC StartSession
- POST /v1/hosts/{id}/estop endpoint (publishes to NATS)
- Worker estop listener wired (was dead code)
- roz estop <host> CLI command

## Test plan
- [ ] cargo build --workspace
- [ ] cargo clippy --workspace -- -D warnings
- [ ] cargo test --workspace --exclude roz-db --exclude roz-server
- [ ] roz --host my-robot connects to Cloud with host_id set
- [ ] roz estop my-robot sends e-stop via REST -> NATS"
```

- [ ] **Step 5: Merge after CI**

```bash
gh pr merge --squash --admin
```
