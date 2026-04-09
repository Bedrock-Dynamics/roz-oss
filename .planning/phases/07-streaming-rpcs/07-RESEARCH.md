# Phase 7: Streaming RPCs - Research

**Researched:** 2026-04-08
**Domain:** gRPC server-streaming RPCs with NATS-backed change notification
**Confidence:** HIGH

## Summary

This phase adds two server-streaming RPCs (`StreamFrameTree`, `WatchCalibration`) to the existing `EmbodimentService`. The codebase already has two production-proven streaming patterns: `StreamTaskStatus` (simple NATS pub/sub, tasks.rs:479-532) and `WatchTeam` (JetStream ordered consumer, agent.rs:281-400). The new RPCs follow the simpler `StreamTaskStatus` pattern since embodiment changes are fire-and-forget notifications, not durable event streams.

The main implementation work is: (1) proto messages for streaming request/response with oneof snapshot/delta/keepalive, (2) NATS change event publication in the `update_embodiment` REST handler, (3) `EmbodimentServiceImpl` gains an `Option<async_nats::Client>` field, (4) two streaming handler implementations with initial snapshot + NATS subscription loop + keepalive timer, (5) server-side diff logic for computing frame tree and calibration deltas.

**Primary recommendation:** Follow the `StreamTaskStatus` pattern exactly (mpsc channel + ReceiverStream + tokio::spawn + NATS subscribe). Add NATS client to `EmbodimentServiceImpl`. Publish change events from the `update_embodiment` handler after adding `State<AppState>` as an extractor parameter.

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- **D-01:** NATS event on PUT. The `PUT /v1/hosts/:id/embodiment` handler publishes a change event to a NATS subject after successful upsert. Streaming handlers subscribe to this subject. Follows the existing `StreamTaskStatus` and `WatchTeam` patterns.
- **D-02:** NATS is required for streaming RPCs. Return `FAILED_PRECONDITION` if NATS is unavailable. Matches the `StreamTaskStatus` contract in `tasks.rs:496-499`. No polling fallback.
- **D-03:** `EmbodimentServiceImpl` gains an `Option<async_nats::Client>` field (same pattern as `TaskServiceImpl`).
- **D-04:** Initial snapshot on connect. First message is always a full snapshot of current state. Client gets hydrated immediately without a separate `GetModel` call. Matches `StreamTaskStatus` which sends initial status before subscribing.
- **D-05:** Delta updates after initial snapshot. Subsequent messages contain only changed `FrameNode`s / removed node IDs (for `StreamFrameTree`) or changed calibration entries (for `WatchCalibration`). Server diffs current JSONB against previous digest to compute deltas.
- **D-06:** Digest-only keepalive heartbeat. Periodic keepalive message with current digest and server timestamp. Client confirms sync without processing a full payload. Satisfies STRM-03.
- **D-07:** oneof payload wrapper pattern. Response messages use `oneof payload { Snapshot snapshot; Delta delta; Keepalive keepalive; }` with `host_id` and `digest` as top-level fields outside the oneof.
- **D-08:** Symmetric design. `WatchCalibration` uses the same oneof pattern as `StreamFrameTree` (snapshot/delta/keepalive) for API consistency.
- **D-09:** Shared `Keepalive` message type. Both RPCs reference the same `Keepalive` message containing `server_time` (google.protobuf.Timestamp) and `digest` (string).
- **D-10:** End stream with error on NATS drop. Send `Status::internal` and close the stream. Client reconnects and gets a fresh snapshot. Matches `WatchTeam` pattern.
- **D-11:** `NOT_FOUND` for both missing host and host-without-embodiment. Same as existing `GetModel` behavior.

### Claude's Discretion
- Keepalive interval timing (likely 15-30s, standard for gRPC server-streaming)
- NATS subject naming convention for embodiment change events
- Server-side diff algorithm for computing FrameTree deltas
- Proto field numbering within new messages
- Test fixture data and integration test structure

### Deferred Ideas (OUT OF SCOPE)
None -- discussion stayed within phase scope
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| STRM-01 | Server can stream frame tree structural changes to connected gRPC clients via StreamFrameTree RPC | StreamTaskStatus pattern provides complete implementation template; proto oneof wrapper per D-07; FrameTree domain type has `all_frame_ids()` + `get_frame()` accessors for snapshot/diff |
| STRM-02 | Server can stream calibration overlay changes to connected gRPC clients via WatchCalibration RPC | Symmetric design per D-08; CalibrationOverlay has BTreeMap fields (joint_offsets, frame_corrections, sensor_calibrations) suitable for entry-level diffing |
| STRM-03 | Streaming responses include digest fields so clients can detect actual data changes vs keepalives | `EmbodimentModel::compute_digest()` and `CalibrationOverlay::compute_digest()` already produce SHA-256 hex digests; shared `Keepalive` message per D-09 |
| STRM-04 | StreamFrameTree sends initial full snapshot then only changed FrameNodes (delta pattern) | Pulled from v2 into this phase per CONTEXT.md; FrameNode equality via `PartialEq` enables per-node diffing between current and previous state |
</phase_requirements>

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| tonic | 0.13 (workspace) | gRPC server-streaming RPC implementation | Already used for all gRPC services [VERIFIED: crates/roz-server/Cargo.toml] |
| prost / prost-types | 0.13 (workspace) | Proto message codegen + Timestamp type | Already used workspace-wide [VERIFIED: crates/roz-server/Cargo.toml] |
| async-nats | 0.38 (workspace) | NATS pub/sub for change notifications | Already used by TaskServiceImpl and AgentServiceImpl [VERIFIED: crates/roz-server/Cargo.toml] |
| tokio-stream | workspace | `ReceiverStream` wrapper for mpsc -> gRPC stream | Already used in StreamTaskStatus and WatchTeam [VERIFIED: grep of imports] |
| tokio | 1 (workspace) | mpsc channels, spawn, interval timer | Already used everywhere [VERIFIED: crates/roz-server/Cargo.toml] |
| serde_json | workspace | Serialize/deserialize NATS payloads | Already used for TaskStatusEvent wire format [VERIFIED: tasks.rs:515] |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| futures | workspace | `StreamExt::next()` for NATS subscription iteration | Already imported in agent.rs and tasks.rs for stream polling [VERIFIED: agent.rs imports] |
| sha2 | workspace | SHA-256 digest computation for change detection | Already used by `EmbodimentModel::compute_digest()` [VERIFIED: model.rs:188] |

### Alternatives Considered
None -- all libraries are already in the workspace dependency set. No new dependencies needed.

## Architecture Patterns

### Recommended Integration Points

```
proto/roz/v1/embodiment.proto          -- add streaming RPCs + new message types
crates/roz-nats/src/dispatch.rs        -- add embodiment change event type + subject fn
crates/roz-server/src/grpc/embodiment.rs -- add NATS field, implement streaming RPCs
crates/roz-server/src/routes/hosts.rs  -- publish NATS event after embodiment upsert
crates/roz-server/src/main.rs          -- pass NATS client to EmbodimentServiceImpl
```

### Pattern 1: Server-Streaming RPC (from StreamTaskStatus)

**What:** mpsc channel + ReceiverStream + tokio::spawn background task
**When to use:** Both StreamFrameTree and WatchCalibration
**Source:** `crates/roz-server/src/grpc/tasks.rs:479-532` [VERIFIED: codebase read]

```rust
// 1. Define stream type alias
type StreamFrameTreeStream = ReceiverStream<Result<StreamFrameTreeResponse, Status>>;

// 2. In the RPC handler:
//    a. Auth + parse host_id
//    b. Check NATS availability (return FAILED_PRECONDITION if None)
//    c. Load current state from DB for initial snapshot
//    d. Subscribe to NATS subject for this host
//    e. Create mpsc channel
//    f. Send initial snapshot through tx
//    g. Spawn background task: NATS sub loop + keepalive timer
//    h. Return ReceiverStream wrapping rx
```

### Pattern 2: NATS Change Event Publication (from estop handler)

**What:** Publish a JSON-serialized event to a per-host NATS subject after DB write
**When to use:** In `update_embodiment` REST handler
**Source:** `crates/roz-server/src/routes/hosts.rs:229-234` [VERIFIED: codebase read]

Key observation: `update_embodiment` currently takes `Tx` and `Extension(auth)` but NOT `State<AppState>`. It needs `State<AppState>` added as an extractor parameter to access `state.nats_client`. The `estop` handler already demonstrates this pattern.

```rust
// In update_embodiment, after conditional_upsert returns true:
if wrote {
    if let Some(nats) = &state.nats_client {
        let event = EmbodimentChangedEvent { host_id: id, tenant_id };
        let payload = serde_json::to_vec(&event).unwrap_or_default();
        let subject = embodiment_changed_subject(id);
        if let Err(e) = nats.publish(subject, payload.into()).await {
            tracing::warn!(error = %e, %id, "failed to publish embodiment change event");
            // Don't fail the request -- the DB write succeeded
        }
    }
    Ok(StatusCode::OK)
}
```

### Pattern 3: Background Streaming Loop with Keepalive

**What:** tokio::spawn task that multiplexes NATS messages and periodic keepalive timer
**When to use:** Both streaming RPC handlers
**Source:** Combines StreamTaskStatus loop (tasks.rs:513-528) with tokio::time::interval for keepalives [VERIFIED + ASSUMED pattern composition]

```rust
tokio::spawn(async move {
    let mut keepalive_interval = tokio::time::interval(Duration::from_secs(15));
    keepalive_interval.tick().await; // skip first immediate tick
    let mut last_digest = initial_digest;

    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(nats_msg) => {
                        // Deserialize change event
                        // Load current state from DB
                        // Diff against last_digest
                        // If changed: compute delta, send through tx, update last_digest
                        // If unchanged: skip (redundant notification)
                    }
                    None => break, // NATS stream closed
                }
            }
            _ = keepalive_interval.tick() => {
                let keepalive = build_keepalive(last_digest.clone());
                if tx.send(Ok(keepalive)).await.is_err() {
                    break; // client disconnected
                }
            }
        }
    }
});
```

### Pattern 4: Delta Computation

**What:** Compare current FrameTree/CalibrationOverlay against previous state to produce delta messages
**When to use:** After NATS notification triggers a DB reload [ASSUMED: reasonable diff approach]

For FrameTree deltas:
- Compare frame-by-frame using `FrameNode: PartialEq`
- Changed: frames present in both old and new but with different values
- Added: frames in new but not old
- Removed: frame IDs in old but not new

For CalibrationOverlay deltas:
- Compare BTreeMap entries (joint_offsets, frame_corrections, sensor_calibrations)
- Changed: entries where value differs
- Added: keys in new but not old
- Removed: keys in old but not new

### Anti-Patterns to Avoid
- **Polling DB instead of NATS subscription:** D-01 and D-02 explicitly require NATS. No polling fallback.
- **Sending full model on every notification:** D-05 requires delta after initial snapshot. Only the initial message is a full snapshot.
- **Using JetStream for change events:** StreamTaskStatus uses simple NATS pub/sub, not JetStream. Embodiment change events are ephemeral notifications -- if no client is listening, the event is lost (client will get fresh snapshot on reconnect). JetStream adds unnecessary complexity.
- **Blocking the REST handler on NATS publish failure:** The DB write is the source of truth. NATS publish failure should be logged as a warning, not returned as an error to the caller.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| gRPC server-stream wiring | Custom stream implementation | `ReceiverStream<Result<T, Status>>` from tokio-stream | Already proven in 3 existing RPCs; handles backpressure via mpsc channel bounds |
| Change notification bus | Custom pub/sub or polling | async-nats `subscribe()` + `publish()` | Already integrated; handles reconnection, buffering, fan-out |
| SHA-256 digest | Custom hashing | `EmbodimentModel::compute_digest()` / `CalibrationOverlay::compute_digest()` | Already implemented and tested; deterministic serialization with BTreeMap key ordering |
| Proto codegen | Manual message builders | tonic-build via existing `build.rs` | Already compiles `embodiment.proto`; new messages auto-generate |

## Common Pitfalls

### Pitfall 1: NATS Subscription Race Condition
**What goes wrong:** Client connects, handler loads DB snapshot, subscribes to NATS. A change happens between DB read and NATS subscribe -- client misses the update.
**Why it happens:** Subscribe-after-read ordering gap.
**How to avoid:** Subscribe to NATS FIRST, then load the DB snapshot. The first NATS message after subscribe will carry the same change, and the diff logic will either detect it as a real delta or skip it (if digest matches). This is the standard pattern for "subscribe then catch up."
**Warning signs:** Stale initial snapshot that doesn't match the first delta.

### Pitfall 2: update_embodiment Needs State<AppState>
**What goes wrong:** Can't access `nats_client` from the `update_embodiment` handler.
**Why it happens:** Handler currently extracts `Tx` + `Extension(auth)` + `Path(id)` + `Json(body)` but not `State<AppState>`. NATS client lives in AppState.
**How to avoid:** Add `State(state): State<AppState>` as an extractor parameter. The `estop` handler (hosts.rs:204) already does this.
**Warning signs:** Compile error on `state.nats_client` access.

### Pitfall 3: Keepalive Timer Drift
**What goes wrong:** Using `tokio::time::sleep` in a loop instead of `tokio::time::interval` causes drift.
**Why it happens:** Sleep-then-work pattern doesn't account for processing time.
**How to avoid:** Use `tokio::time::interval(Duration::from_secs(15))` and `tick().await` in a `tokio::select!` loop.
**Warning signs:** Keepalive interval grows over time.

### Pitfall 4: Unbounded Memory from Delta State
**What goes wrong:** Keeping the full previous FrameTree/CalibrationOverlay in memory per-stream for diffing.
**Why it happens:** Delta computation requires comparing current vs previous state.
**How to avoid:** This is actually fine for embodiment data -- a FrameTree is at most dozens of nodes (not megabytes). Store the previous state in the spawned task's local scope. The memory cost is negligible per client.
**Warning signs:** None expected -- this is a non-issue for structural (not telemetry) data.

### Pitfall 5: Missing Tenant Isolation on NATS Subject
**What goes wrong:** Client subscribes to NATS subject for a host belonging to a different tenant.
**Why it happens:** NATS subjects are host_id-scoped, not tenant-scoped.
**How to avoid:** Verify tenant ownership in the RPC handler BEFORE subscribing (same as StreamTaskStatus verifies task ownership at lines 489-494). The initial DB load performs this check via `fetch_embodiment_row`.
**Warning signs:** Cross-tenant data leakage.

### Pitfall 6: NATS Publish After Transaction Commit
**What goes wrong:** Publishing to NATS before the DB transaction commits -- if the transaction rolls back, listeners process a phantom event.
**Why it happens:** The `update_embodiment` handler uses the `Tx` middleware which auto-commits on success response. NATS publish happens in the handler body.
**How to avoid:** Publish AFTER the conditional_upsert succeeds but note that the Tx middleware commits the transaction AFTER the handler returns OK. This means the NATS event could fire before commit. In practice this is acceptable because: (1) the streaming handler re-reads from DB and will get the committed state or the old state (both valid), (2) if the old state is read, the digest won't change and no delta will be sent. However, be aware of this ordering.
**Warning signs:** Extremely rare: NATS event arrives, subscriber reads DB, gets old state. Next real change will correct this.

## Code Examples

### Example 1: Proto Streaming Messages (D-07, D-08, D-09)

```protobuf
// Source: based on decisions D-07, D-08, D-09 + existing embodiment.proto patterns
// [VERIFIED: existing oneof patterns in embodiment.proto:284-289, 358-362]

// Shared keepalive message for all embodiment streams.
message Keepalive {
  google.protobuf.Timestamp server_time = 1;
  string digest = 2;
}

// --- StreamFrameTree ---

message StreamFrameTreeRequest {
  string host_id = 1;
}

message StreamFrameTreeResponse {
  string host_id = 1;
  string digest = 2;
  oneof payload {
    FrameTreeSnapshot snapshot = 3;
    FrameTreeDelta delta = 4;
    Keepalive keepalive = 5;
  }
}

message FrameTreeSnapshot {
  FrameTree frame_tree = 1;
}

message FrameTreeDelta {
  map<string, FrameNode> changed_frames = 1;
  repeated string removed_frame_ids = 2;
  optional string new_root = 3;
}

// --- WatchCalibration ---

message WatchCalibrationRequest {
  string host_id = 1;
}

message WatchCalibrationResponse {
  string host_id = 1;
  string digest = 2;
  oneof payload {
    CalibrationSnapshot snapshot = 3;
    CalibrationDelta delta = 4;
    Keepalive keepalive = 5;
  }
}

message CalibrationSnapshot {
  CalibrationOverlay calibration = 1;
}

message CalibrationDelta {
  map<string, double> changed_joint_offsets = 1;
  repeated string removed_joint_offsets = 2;
  map<string, Transform3D> changed_frame_corrections = 3;
  repeated string removed_frame_corrections = 4;
  map<string, SensorCalibration> changed_sensor_calibrations = 5;
  repeated string removed_sensor_calibrations = 6;
}
```

### Example 2: EmbodimentServiceImpl with NATS (D-03)

```rust
// Source: follows TaskServiceImpl pattern [VERIFIED: tasks.rs:19-30]
pub struct EmbodimentServiceImpl {
    pool: PgPool,
    auth: std::sync::Arc<dyn GrpcAuth>,
    nats_client: Option<async_nats::Client>,  // NEW
}

impl EmbodimentServiceImpl {
    pub fn new(
        pool: PgPool,
        auth: std::sync::Arc<dyn GrpcAuth>,
        nats_client: Option<async_nats::Client>,
    ) -> Self {
        Self { pool, auth, nats_client }
    }
}
```

### Example 3: NATS Change Event Type

```rust
// Source: follows TaskStatusEvent pattern [VERIFIED: dispatch.rs:28-36]
// Recommended location: crates/roz-nats/src/dispatch.rs (alongside TaskStatusEvent)

pub const INTERNAL_EMBODIMENT_CHANGED_PREFIX: &str = "roz.internal.embodiment.changed";

/// NATS subject for embodiment change notifications for a specific host.
#[must_use]
pub fn embodiment_changed_subject(host_id: Uuid) -> String {
    format!("{INTERNAL_EMBODIMENT_CHANGED_PREFIX}.{host_id}")
}

/// Wire event published when a host's embodiment data changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbodimentChangedEvent {
    pub host_id: Uuid,
    pub tenant_id: Uuid,
}
```

### Example 4: Streaming RPC Handler (StreamFrameTree)

```rust
// Source: follows StreamTaskStatus pattern [VERIFIED: tasks.rs:479-532]
type StreamFrameTreeStream = ReceiverStream<Result<StreamFrameTreeResponse, Status>>;

async fn stream_frame_tree(
    &self,
    request: Request<StreamFrameTreeRequest>,
) -> Result<Response<Self::StreamFrameTreeStream>, Status> {
    let tenant_id = self.authenticated_tenant_id(&request).await?;
    let host_id = parse_host_id(&request.get_ref().host_id)?;

    // Verify NATS availability (D-02)
    let nats = self.nats_client.as_ref()
        .ok_or_else(|| Status::failed_precondition("frame tree streaming requires NATS"))?
        .clone();

    // Subscribe FIRST (Pitfall 1: avoid race condition)
    let subject = roz_nats::dispatch::embodiment_changed_subject(host_id);
    let mut sub = nats.subscribe(subject).await.map_err(|e| {
        tracing::error!(error = %e, %host_id, "failed to subscribe to embodiment changes");
        Status::internal("failed to subscribe to embodiment changes")
    })?;

    // Load current state for initial snapshot (D-04)
    let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;
    // ... deserialize, build snapshot message ...

    let (tx, rx) = tokio::sync::mpsc::channel(32);
    // Send initial snapshot
    tx.send(Ok(snapshot_response)).await
        .map_err(|_| Status::internal("failed to initialize stream"))?;

    let pool = self.pool.clone();
    tokio::spawn(async move {
        // Background loop: NATS events + keepalive timer
        // See Pattern 3 above
    });

    Ok(Response::new(ReceiverStream::new(rx)))
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Full model on every stream message | Initial snapshot + deltas (D-05) | This phase | Reduces bandwidth for large models with small changes |
| Separate GetModel + subscribe | Initial snapshot in stream (D-04) | This phase | One RPC call for full hydration instead of two |

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | 15-second keepalive interval is appropriate for gRPC server-streaming | Architecture Patterns (Pattern 3) | LOW -- easily adjusted; no client-side dependency on exact interval |
| A2 | NATS subject format `roz.internal.embodiment.changed.{host_id}` follows existing conventions | Code Examples (Example 3) | LOW -- naming is discretionary per CONTEXT.md |
| A3 | FrameNode-level diffing is sufficient for delta computation (no sub-field diffing within a node) | Architecture Patterns (Pattern 4) | LOW -- FrameNode is small (4 fields); replacing entire node on any change is acceptable |
| A4 | Storing previous FrameTree/CalibrationOverlay in the spawned task for diffing is acceptable memory-wise | Pitfalls (Pitfall 4) | LOW -- structural data is small (tens of KB at most) |
| A5 | Publishing NATS event before Tx middleware commits is acceptable due to re-read-from-DB pattern | Pitfalls (Pitfall 6) | LOW -- worst case is a no-op diff cycle; self-correcting on next real change |

## Open Questions (RESOLVED)

1. **CalibrationOverlay absence handling in WatchCalibration**
   - What we know: A host may have a model but no calibration overlay (`calibration` field is optional in `EmbodimentRuntime`)
   - What's unclear: Should `WatchCalibration` return `NOT_FOUND` if no calibration exists, or return a snapshot with empty/null calibration?
   - **RESOLVED:** Return `NOT_FOUND` consistent with D-11 pattern (host-without-embodiment returns NOT_FOUND). If calibration is later added, client reconnects and gets snapshot.

2. **Delta for calibration top-level scalar fields**
   - What we know: `CalibrationOverlay` has BTreeMap fields (easily diffable) but also scalar fields like `calibration_id`, `calibrated_at`, `temperature_range`
   - What's unclear: Should delta include changed scalars, or only map entries?
   - **RESOLVED:** If the digest changes, any scalar change means a full recalibration happened. Include `calibration_id` and `calibrated_at` in the delta message as optional fields so clients know which calibration version produced the delta.

## Environment Availability

Step 2.6: SKIPPED (no external dependencies identified -- phase is purely code/config changes to existing crates using existing workspace dependencies).

## Security Domain

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | yes | Existing `authenticated_tenant_id()` on EmbodimentServiceImpl [VERIFIED: embodiment.rs:42-56] |
| V3 Session Management | no | Stateless gRPC streaming -- no session tokens |
| V4 Access Control | yes | Tenant isolation via `fetch_embodiment_row` + tenant_id check [VERIFIED: embodiment.rs:72-92] |
| V5 Input Validation | yes | UUID parsing via `parse_host_id` [VERIFIED: embodiment.rs:64-68] |
| V6 Cryptography | no | Digest is for change detection, not security |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Cross-tenant data leakage via NATS subject guessing | Information Disclosure | Verify tenant ownership in RPC handler before subscribing (same as StreamTaskStatus) [VERIFIED: tasks.rs:489-494] |
| Denial of service via many open streams | Denial of Service | mpsc channel bound (32) provides backpressure; tonic connection limits [ASSUMED: standard tonic behavior] |
| Stale data injection via NATS publish before commit | Tampering | Re-read from DB on every NATS notification; digest comparison rejects stale reads |

## Sources

### Primary (HIGH confidence)
- `crates/roz-server/src/grpc/tasks.rs:479-532` -- StreamTaskStatus complete pattern (mpsc + ReceiverStream + NATS)
- `crates/roz-server/src/grpc/agent.rs:281-400` -- WatchTeam pattern (JetStream variant)
- `crates/roz-server/src/grpc/embodiment.rs` -- current EmbodimentServiceImpl structure
- `proto/roz/v1/embodiment.proto` -- existing service definition and message types
- `crates/roz-server/src/routes/hosts.rs:155-202` -- update_embodiment handler (NATS event source)
- `crates/roz-nats/src/dispatch.rs` -- NATS subject conventions and wire types
- `crates/roz-core/src/embodiment/model.rs` -- EmbodimentModel with compute_digest()
- `crates/roz-core/src/embodiment/calibration.rs` -- CalibrationOverlay with BTreeMap fields
- `crates/roz-core/src/embodiment/frame_tree.rs` -- FrameTree, FrameNode (PartialEq, BTreeMap-backed)
- `crates/roz-server/src/state.rs` -- AppState with nats_client field

### Secondary (MEDIUM confidence)
- `crates/roz-server/src/main.rs:128-131` -- current EmbodimentServiceImpl::new() call site (needs NATS param added)
- `crates/roz-nats/src/subjects.rs` -- NATS subject naming conventions

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH -- all libraries already in workspace, patterns already proven in codebase
- Architecture: HIGH -- direct copy of existing StreamTaskStatus + WatchTeam patterns
- Pitfalls: HIGH -- identified from reading actual codebase implementation details
- Delta logic: MEDIUM -- diff algorithm is straightforward but novel code (no existing delta pattern in codebase)

**Research date:** 2026-04-08
**Valid until:** 2026-05-08 (stable domain -- proto + NATS patterns unlikely to change)
