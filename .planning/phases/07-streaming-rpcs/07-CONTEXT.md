# Phase 7: Streaming RPCs - Context

**Gathered:** 2026-04-09
**Status:** Ready for planning

<domain>
## Phase Boundary

Add `StreamFrameTree` and `WatchCalibration` server-streaming RPCs to `EmbodimentService`. Connected clients receive real-time updates when a host's frame tree structure or calibration overlays change, with initial snapshot, subsequent delta updates, and digest-based keepalives.

**Scope note:** STRM-04 (delta pattern after initial snapshot) is pulled into this phase from the v2 backlog. Both RPCs send an initial full snapshot, then only changed data on subsequent messages.

</domain>

<decisions>
## Implementation Decisions

### Change Notification Mechanism
- **D-01:** NATS event on PUT. The `PUT /v1/hosts/:id/embodiment` handler publishes a change event to a NATS subject after successful upsert. Streaming handlers subscribe to this subject. Follows the existing `StreamTaskStatus` and `WatchTeam` patterns.
- **D-02:** NATS is required for streaming RPCs. Return `FAILED_PRECONDITION` if NATS is unavailable. Matches the `StreamTaskStatus` contract in `tasks.rs:496-499`. No polling fallback.
- **D-03:** `EmbodimentServiceImpl` gains an `Option<async_nats::Client>` field (same pattern as `TaskServiceImpl`).

### Stream Content
- **D-04:** Initial snapshot on connect. First message is always a full snapshot of current state. Client gets hydrated immediately without a separate `GetModel` call. Matches `StreamTaskStatus` which sends initial status before subscribing.
- **D-05:** Delta updates after initial snapshot. Subsequent messages contain only changed `FrameNode`s / removed node IDs (for `StreamFrameTree`) or changed calibration entries (for `WatchCalibration`). Server diffs current JSONB against previous digest to compute deltas.
- **D-06:** Digest-only keepalive heartbeat. Periodic keepalive message with current digest and server timestamp. Client confirms sync without processing a full payload. Satisfies STRM-03.

### Proto Design
- **D-07:** oneof payload wrapper pattern. Response messages use `oneof payload { Snapshot snapshot; Delta delta; Keepalive keepalive; }` with `host_id` and `digest` as top-level fields outside the oneof. Type-safe, no ambiguous optional fields. Matches existing oneof pattern for Rust enums with data.
- **D-08:** Symmetric design. `WatchCalibration` uses the same oneof pattern as `StreamFrameTree` (snapshot/delta/keepalive) for API consistency.
- **D-09:** Shared `Keepalive` message type. Both RPCs reference the same `Keepalive` message containing `server_time` (google.protobuf.Timestamp) and `digest` (string).

### Failure Handling
- **D-10:** End stream with error on NATS drop. Send `Status::internal` and close the stream. Client reconnects and gets a fresh snapshot. Matches `WatchTeam` pattern where NATS errors break the forwarding loop.
- **D-11:** `NOT_FOUND` for both missing host and host-without-embodiment. Same as existing `GetModel` behavior, follows Phase 6 D-09 pattern.

### Claude's Discretion
- Keepalive interval timing (likely 15-30s, standard for gRPC server-streaming)
- NATS subject naming convention for embodiment change events
- Server-side diff algorithm for computing FrameTree deltas
- Proto field numbering within new messages
- Test fixture data and integration test structure

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Existing Streaming RPC Patterns
- `crates/roz-server/src/grpc/tasks.rs` lines 479-532 -- `StreamTaskStatus` implementation: mpsc + ReceiverStream + NATS subscription pattern
- `crates/roz-server/src/grpc/agent.rs` lines 281-400 -- `WatchTeam` implementation: NATS → mpsc → ReceiverStream with error handling

### Embodiment Service
- `crates/roz-server/src/grpc/embodiment.rs` -- `EmbodimentServiceImpl` struct, auth pattern, `parse_host_id`, `load_embodiment_row`
- `crates/roz-server/src/grpc/embodiment_convert.rs` -- domain ↔ proto conversions, existing roundtrip proptests

### Proto Definitions
- `proto/roz/v1/embodiment.proto` -- existing service definition, message types, oneof patterns for enums
- `proto/roz/v1/tasks.proto` lines 13, 53 -- `StreamTaskStatus` RPC and request message (pattern reference)
- `proto/roz/v1/agent.proto` lines 14-15 -- `WatchTeam` RPC declaration (pattern reference)

### Domain Types
- `crates/roz-core/src/embodiment/frame_tree.rs` -- `FrameTree`, `FrameNode`, `Transform3D`
- `crates/roz-core/src/embodiment/calibration.rs` -- `CalibrationOverlay`, `SensorCalibration`
- `crates/roz-core/src/embodiment/model.rs` -- `EmbodimentModel`, `compute_digest()`, `stamp_digest()`

### Write Path (Change Event Source)
- `crates/roz-server/src/routes/hosts.rs` lines 155-173 -- `PUT /v1/hosts/:id/embodiment` handler where NATS change event must be published
- `crates/roz-db/src/embodiments.rs` -- `upsert()`, `conditional_upsert()`, `get_by_host_id()`

### Server State
- `crates/roz-server/src/state.rs` -- `AppState` with optional NATS client pattern

### Requirements
- `.planning/REQUIREMENTS.md` -- STRM-01, STRM-02, STRM-03 requirements; STRM-04 (delta pattern) pulled into this phase

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- `StreamTaskStatus` (tasks.rs:479-532): Complete server-streaming pattern with mpsc + ReceiverStream + NATS subscription + initial message
- `WatchTeam` (agent.rs:281-400): Alternative streaming pattern with NATS JetStream consumer
- `EmbodimentServiceImpl`: Auth, tenant isolation, host lookup, JSONB deserialization already working
- `FrameTree`, `CalibrationOverlay` domain types with serde support
- `EmbodimentModel::compute_digest()` / `stamp_digest()`: SHA-256 digest for change detection

### Established Patterns
- Server-streaming: `type XxxStream = ReceiverStream<Result<Proto, Status>>` + `tokio::spawn` background task
- NATS subscription: `nats.subscribe(subject)` → forward messages through mpsc sender
- Initial snapshot: Send first message before entering the NATS subscription loop
- Error mapping: serde errors → `Status::internal()` with sanitized messages

### Integration Points
- `proto/roz/v1/embodiment.proto` -- add new RPC declarations and streaming message types
- `crates/roz-server/src/grpc/embodiment.rs` -- add NATS field, implement streaming RPC handlers
- `crates/roz-server/src/routes/hosts.rs` -- publish NATS change event after embodiment upsert
- `crates/roz-server/src/state.rs` -- pass NATS client through to `EmbodimentServiceImpl`
- `crates/roz-server/build.rs` -- no changes needed (already compiles all of embodiment.proto)

</code_context>

<specifics>
## Specific Ideas

- Proto message shape uses oneof payload wrapper with digest/host_id at top level (see D-07 preview in discussion)
- STRM-04 delta pattern explicitly pulled from v2 into this phase per user decision

</specifics>

<deferred>
## Deferred Ideas

None -- discussion stayed within phase scope

</deferred>

---

*Phase: 07-streaming-rpcs*
*Context gathered: 2026-04-09*
