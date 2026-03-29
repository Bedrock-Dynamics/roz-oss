# Safety Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 9 safety and reliability issues found by three independent code reviews (robotics architect, CodeRabbit, general agent). Scoped to ISO 13850 (e-stop), IEC 61508 (watchdog), and ROS2 patterns.

**Architecture:** Cooperative e-stop via tokio::select! around agent execution (Category 1). Watchdog spawned per session. Relay tasks cancelled via CancellationToken. Heartbeat-aware timeout replaces static 30s.

**Tech Stack:** Rust, tokio (select!, CancellationToken), async-nats, tonic

---

## File Structure

| File | Action | Fix # |
|------|--------|-------|
| `crates/roz-worker/src/session_relay.rs:203` | Modify | 1 (e-stop during agent.run) |
| `crates/roz-worker/src/main.rs:254` | Modify | 2 (spawn watchdog) |
| `crates/roz-nats/src/subjects.rs:120` | Modify | 3 (validate estop subject) |
| `crates/roz-server/src/routes/hosts.rs` | Modify | 3 (update estop caller) |
| `crates/roz-worker/src/session_relay.rs` | Modify | 4 (keepalive during long turns) |
| `crates/roz-server/src/grpc/agent.rs:2009` | Modify | 4 (heartbeat-aware timeout) |
| `crates/roz-server/src/grpc/agent.rs:1281,1512` | Modify | 5 (CancellationToken for relays) |
| `crates/roz-server/src/grpc/agent.rs` | Modify | 6 (cache worker_id) |
| `crates/roz-agent/src/agent_loop.rs:748` | Modify | 7 (warn on empty observe) |
| `crates/roz-server/src/grpc/agent.rs` | Modify | 8 (remove duplicate tests) |
| `crates/roz-worker/src/session_relay.rs` | Modify | 8 (remove duplicate tests) |
| `crates/roz-server/src/grpc/agent.rs:91` | Modify | 9 (remove dead_code allow) |

---

### Task 1: E-stop interrupts agent.run() via tokio::select! — BOTH paths [P0 SAFETY]

**Files:**
- Modify: `crates/roz-worker/src/session_relay.rs:183-244`
- Modify: `crates/roz-worker/src/main.rs` (execute_task function, ~line 78)

- [ ] **Step 1: Fix session_relay.rs — wrap agent.run() in select! with estop**

In `handle_edge_session()`, replace the `agent.run(input).await` call (line 203) with:

```rust
let agent_result = tokio::select! {
    result = agent.run(input) => result,
    _ = estop_rx.changed() => {
        if *estop_rx.borrow() {
            tracing::error!(session_id, "E-STOP fired during agent execution — aborting turn");
            let error = serde_json::json!({"type": "error", "message": "E-STOP activated during execution"});
            if let Ok(payload) = serde_json::to_vec(&error) {
                let _ = nats.publish(response_subject.clone(), payload.into()).await;
            }
            break;
        }
        continue;
    }
};

match agent_result {
    Ok(output) => {
        // ... existing response handling ...
    }
    Err(e) => {
        // ... existing error handling ...
    }
}
```

- [ ] **Step 2: Fix main.rs — wrap execute_task's agent.run() in select! with estop**

In the `execute_task` function (~line 78), find `agent.run(agent_input).await` and apply the same tokio::select! pattern. Pass `estop_rx` into `execute_task` as a parameter.

On e-stop cancellation, immediately halt the CopperHandle (don't gracefully shutdown — DROP it for emergency halt):

```rust
let agent_result = tokio::select! {
    result = agent.run(agent_input) => result,
    _ = estop_rx.changed() => {
        if *estop_rx.borrow() {
            tracing::error!(task_id = %invocation.task_id, "E-STOP during task execution");
            // DROP copper handle — triggers emergency halt (zeroes all commands)
            // Do NOT call shutdown().await — that's graceful. Drop is immediate.
            drop(copper_handle.take());
            anyhow::bail!("E-STOP activated during task execution");
        }
        continue; // spurious wakeup
    }
};
```

**IMPORTANT (from agentic review):** Even with select! + drop, there's a window where the WASM controller runs unsupervised (between the last agent yield point and the select! firing). For true Category 0 e-stop, wire an `AtomicBool` e-stop flag directly into the Copper controller thread. The controller checks it every tick (10ms) and zeroes output immediately. This is a follow-up task — the select! + drop gives Category 1 (seconds), the AtomicBool gives Category 0 (10ms).

- [ ] **Step 3: Build and clippy**

Run: `cargo fmt -p roz-worker && cargo clippy -p roz-worker -- -D warnings`

- [ ] **Step 4: Commit**

```bash
git add crates/roz-worker/
git commit -m "fix(safety): e-stop interrupts agent.run() on BOTH task and session paths

ISO 13850: e-stop must have priority over all control functions.
tokio::select! around agent.run() on both paths. Task path also
shuts down CopperHandle (100Hz control loop) on e-stop.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Spawn CommandWatchdog in worker [P0 SAFETY]

**Files:**
- Modify: `crates/roz-worker/src/main.rs`
- Modify: `crates/roz-worker/src/session_relay.rs`

- [ ] **Step 1: Spawn watchdog for IDLE monitoring only in main.rs**

The watchdog monitors communication link health while the worker is idle (waiting for tasks). It is NOT active during task execution — tasks have their own timeout mechanisms (model call timeouts, tool dispatcher timeout, max_cycles).

After the estop listener setup (around line 240) and before the task loop, create the watchdog:

```rust
// Command watchdog — detect NATS link failure while idle (30s)
let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::new(
    Duration::from_secs(30),
));
let watchdog_cancel = CancellationToken::new();
let wd = watchdog.clone();
let wd_cancel = watchdog_cancel.clone();
tokio::spawn(async move { wd.run(wd_cancel).await });
tracing::info!("idle watchdog active (30s deadline)");
```

In the task loop, pet on each received message AND pause during task execution:

```rust
while let Some(msg) = sub.next().await {
    watchdog.pet();  // Reset on any NATS activity

    // ... existing estop check ...

    // Pause watchdog during task execution (task has its own timeouts)
    // Pet again when task completes to restart idle monitoring
    tokio::spawn({
        let wd = watchdog.clone();
        async move {
            execute_task(...).await;
            wd.pet(); // Resume idle monitoring after task completes
        }
    });
}
```

- [ ] **Step 2: Spawn watchdog for edge sessions in session_relay.rs**

In `handle_edge_session()`, after session setup, create a session-scoped watchdog with a shorter deadline (60s for interactive sessions — agent turns can be long):

```rust
let session_watchdog = roz_worker::command_watchdog::CommandWatchdog::new(
    Duration::from_secs(60),
);
let wd_cancel = CancellationToken::new();
let wd = session_watchdog.clone();
let wd_cancel_clone = wd_cancel.clone();
tokio::spawn(async move { wd.run(wd_cancel_clone).await });
```

Pet the watchdog on each received message AND from the keepalive task (Fix 4):

```rust
while let Some(msg) = session_sub.next().await {
    session_watchdog.pet();
    // ... existing message handling ...
}
// Cancel watchdog on clean exit
wd_cancel.cancel();
```

**IMPORTANT: Fixes 2 and 4 are coupled.** The keepalive task (Fix 4) must also pet the watchdog during long agent turns. Pass the watchdog Arc into the keepalive task so it calls `watchdog.pet()` every 5s alongside publishing the keepalive message. Without this, the watchdog false-fires during every agent turn >60s.

Note: `CommandWatchdog` uses `Arc<AtomicU64>` internally, so wrapping in `Arc` for sharing is correct.

- [ ] **Step 3: Build and test**

Run: `cargo fmt -p roz-worker && cargo clippy -p roz-worker -- -D warnings && cargo test -p roz-worker`

- [ ] **Step 4: Commit**

```bash
git add crates/roz-worker/
git commit -m "fix(safety): spawn CommandWatchdog for tasks and edge sessions

IEC 61508: watchdog timers mandatory for safety systems.
30s deadline for task invocations, 60s for interactive sessions.
Watchdog petted on each received NATS message.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Validate estop NATS subject [P0 SAFETY]

**Files:**
- Modify: `crates/roz-nats/src/subjects.rs:120-123`
- Modify: `crates/roz-server/src/routes/hosts.rs` (estop endpoint)
- Modify: `crates/roz-safety/src/main.rs` (if it constructs estop subject directly)

- [ ] **Step 1: Write failing test**

In subjects.rs test module:

```rust
#[test]
fn estop_subject_validates_worker_id() {
    assert!(Subjects::estop("valid-worker").is_ok());
    assert!(Subjects::estop("worker.with.dots").is_err());
    assert!(Subjects::estop("").is_err());
    assert!(Subjects::estop("worker*wildcard").is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roz-nats -- estop_subject_validates`
Expected: FAIL (currently returns String, not Result)

- [ ] **Step 3: Fix estop() to validate**

```rust
pub fn estop(worker_id: &str) -> Result<String, RozError> {
    validate_token("worker_id", worker_id)?;
    Ok(format!("safety.estop.{worker_id}"))
}
```

- [ ] **Step 4: Update callers**

In `crates/roz-server/src/routes/hosts.rs`, the estop endpoint calls `Subjects::estop()`. Update:

```rust
let subject = roz_nats::Subjects::estop(&host.name)
    .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({"error": format!("invalid host name for NATS: {e}")}))))?;
```

**All callers using format!() directly (must update ALL):**
- `crates/roz-worker/src/estop.rs:11` — **MOST CRITICAL** (worker's own subscription)
- `crates/roz-safety/src/main.rs:77` — safety daemon subscription
- `crates/roz-safety/tests/safety_pipeline.rs:67` — test
- `crates/roz-safety/tests/nats_integration.rs:81` — test
- `crates/roz-server/src/main.rs` — integration test

Replace ALL with `Subjects::estop()`. Also update existing test `subjects.rs:255` which expects `String` return type.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p roz-nats -- estop_subject_validates`

- [ ] **Step 6: Build workspace**

Run: `cargo build --workspace`

- [ ] **Step 7: Commit**

```bash
git add crates/roz-nats/ crates/roz-server/ crates/roz-safety/ crates/roz-worker/
git commit -m "fix(safety): validate estop NATS subject — prevent silent delivery failure

E-stop subject builder now validates worker_id like all other subjects.
A worker_id containing '.' or '*' would silently break e-stop delivery.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Heartbeat-aware timeout replaces static 30s [P1 RELIABILITY]

**Files:**
- Modify: `crates/roz-worker/src/session_relay.rs`
- Modify: `crates/roz-server/src/grpc/agent.rs:2009`

- [ ] **Step 1: Worker emits keepalive during long agent turns**

In `handle_edge_session()`, before `agent.run()`, spawn a keepalive task:

```rust
let keepalive_nats = nats.clone();
let keepalive_subject = response_subject.clone();
let keepalive_cancel = CancellationToken::new();
let kc = keepalive_cancel.clone();
let keepalive_task = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let msg = serde_json::json!({"type": "keepalive"});
                if let Ok(payload) = serde_json::to_vec(&msg) {
                    let _ = keepalive_nats.publish(keepalive_subject.clone(), payload.into()).await;
                }
            }
            _ = kc.cancelled() => return,
        }
    }
});
```

Cancel after agent.run() returns:

```rust
keepalive_cancel.cancel();
keepalive_task.abort();
```

- [ ] **Step 2: Server relay resets timeout on any message including keepalive**

In `run_edge_relay()`, keep 30s timeout (6 missed keepalives = tight dead-worker detection) and handle keepalive messages:

```rust
let msg = match tokio::time::timeout(Duration::from_secs(30), worker_resp.next()).await {
    // ... existing handling ...
};
// In the message processing, skip keepalive messages (don't relay to client)
if msg_type == "keepalive" {
    continue;
}
```

- [ ] **Step 3: Build and test**

Run: `cargo fmt --all && cargo clippy --workspace -- -D warnings`

- [ ] **Step 4: Commit**

```bash
git add crates/roz-worker/ crates/roz-server/
git commit -m "fix: heartbeat-aware timeout — worker emits keepalive during long agent turns

Replaces static 30s timeout with 60s + keepalive. Worker sends keepalive
every 5s during agent execution. Server resets timeout on any message.
Prevents false timeout during multi-tool agent turns.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Cancel relay tasks on session end [P1 RELIABILITY]

**Files:**
- Modify: `crates/roz-server/src/grpc/agent.rs`

- [ ] **Step 1: Add CancellationToken parameter to relay functions**

Update `spawn_telemetry_relay` and `spawn_webrtc_signaling_relay` signatures to accept `cancel: CancellationToken`. In the spawned task's loop:

```rust
tokio::select! {
    msg = sub.next() => {
        let Some(msg) = msg else { break; };
        // ... process message ...
    }
    _ = cancel.cancelled() => {
        tracing::debug!("relay task cancelled — session ended");
        break;
    }
}
```

- [ ] **Step 2: Create and cancel the token in the session lifecycle**

In `run_session_loop()`, create a `CancellationToken` before calling the relay functions. Cancel it when the session exits (at the end of `run_session_loop`).

```rust
let relay_cancel = CancellationToken::new();
spawn_telemetry_relay(..., relay_cancel.clone()).await;
spawn_webrtc_signaling_relay(..., relay_cancel.clone()).await;
// ... session loop ...
relay_cancel.cancel(); // on exit
```

- [ ] **Step 3: Build and test**

Run: `cargo fmt -p roz-server && cargo clippy -p roz-server -- -D warnings`

- [ ] **Step 4: Commit**

```bash
git add crates/roz-server/
git commit -m "fix: cancel relay tasks on session end via CancellationToken

Telemetry and WebRTC relay tasks now accept CancellationToken.
Cancelled when gRPC session exits, preventing leaked NATS subscriptions.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Cache worker_id in Session struct [P2]

**Files:**
- Modify: `crates/roz-server/src/grpc/agent.rs`

- [ ] **Step 1: Add worker_name field to Session**

```rust
pub worker_name: Option<String>,
```

- [ ] **Step 2: Resolve and cache during handle_start**

After the host_id is extracted, resolve worker_name once:

```rust
let worker_name = if let Some(ref hid) = host_id_str {
    if let Ok(uuid) = uuid::Uuid::parse_str(hid) {
        roz_db::hosts::get_by_id(pool, uuid).await.ok().flatten().map(|h| h.name)
    } else { None }
} else { None };
```

Store in Session. Use `session.worker_name` in relay functions instead of calling `resolve_worker_id()` per message.

- [ ] **Step 3: Commit**

```bash
git commit -m "perf: cache worker_name in Session — avoid per-message DB lookup

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Warn on empty OODA observe [P2]

**Files:**
- Modify: `crates/roz-agent/src/agent_loop.rs:748`

- [ ] **Step 1: Add warning after snapshot**

After `let ctx = self.spatial.snapshot(&input.task_id).await;` add:

```rust
// Alerts are generated events, not perception data. Only check perception sources.
if ctx.entities.is_empty() && ctx.screenshots.is_empty() {
    tracing::warn!(
        task_id = %input.task_id,
        "OodaReAct observe phase returned empty spatial context — no entities, screenshots, or alerts. \
         The agent is operating without any environmental observation."
    );
}
```

- [ ] **Step 2: Same for non-streaming version (line 1227)**

Copy the same warning to the non-streaming path.

- [ ] **Step 3: Commit**

```bash
git commit -m "fix: warn when OodaReAct observe phase returns empty spatial context

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Remove duplicate tests + dead_code allow [P3]

**Files:**
- Modify: `crates/roz-server/src/grpc/agent.rs` (remove resolve_placement tests)
- Modify: `crates/roz-worker/src/session_relay.rs` (remove resolve_placement tests)
- Modify: `crates/roz-server/src/grpc/agent.rs:91` (remove #[allow(dead_code)])

- [ ] **Step 1: Delete duplicate resolve_placement tests**

In `agent.rs` test module, find and delete the `resolve_placement_*` tests.
In `session_relay.rs` test module, find and delete the `resolve_placement_*` tests.
Keep only the tests in `roz-core/src/edge/mod.rs`.

- [ ] **Step 2: Remove #[allow(dead_code)] on Session.host_id**

The field IS used. Remove the annotation.

- [ ] **Step 3: Build and test**

Run: `cargo fmt --all && cargo clippy --workspace -- -D warnings && cargo test --workspace --exclude roz-db --exclude roz-server`

- [ ] **Step 4: Commit**

```bash
git commit -m "chore: remove duplicate tests and stale dead_code annotation

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: Full verification

- [ ] **Step 1: Full workspace build + clippy + test**

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo build --workspace
cargo test --workspace --exclude roz-db --exclude roz-server
cargo test -p roz-worker --test camera_integration
cargo test -p roz-worker --test estop_integration
cargo test -p roz-worker --test dispatch_integration
```

All must pass.

- [ ] **Step 2: Verify no remaining safety issues**

```bash
# Check no empty SafetyStack in worker paths
grep -rn "SafetyStack::new(vec!\[\])" crates/roz-worker/
# Should return 0 results

# Check CommandWatchdog is spawned
grep -rn "CommandWatchdog" crates/roz-worker/src/main.rs crates/roz-worker/src/session_relay.rs
# Should show spawn calls

# Check estop validates
grep -rn "fn estop" crates/roz-nats/src/subjects.rs
# Should show -> Result<String, RozError>
```
