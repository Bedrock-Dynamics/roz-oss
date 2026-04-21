---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 05
subsystem: server
tags: [observability, ingestion, cloud-sessions, telemetry, mcap, fan-in]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    provides: "WriterActor + channels + task-lifecycle sink (26-04); schema_registry + projection (26-03); persistence layer (26-02)"
provides:
  - "crates/roz-server/src/state.rs::AppState — 4 new fields (active_writers, task_lifecycle_sink, schema_descriptors, mcap_dir)"
  - "crates/roz-server/src/observability/ingest_cloud::spawn_cloud_ingestors — 3 fan-in tasks into a per-session WriterActor mpsc::Sender"
  - "crates/roz-server/src/observability/ingest_cloud::{emit_session_event, encode_session_event_proto, spawn_session_telemetry_ingest, test_mcap_args}"
  - "crates/roz-server/src/nats_handlers::verify_telemetry_inbound — factored helper reused by session-scoped telemetry ingest"
  - "crates/roz-server/src/grpc/agent.rs — cloud session branch spawns WriterActor + 3 ingestors at StartSession; finalizes on session end"
affects:
  - 26-06-session-event-wiring (edge path reuses spawn_session_telemetry_ingest)
  - 26-07-sigterm-drain-and-recovery-scan (iterates AppState.active_writers)
  - 26-08-task-lifecycle-db-hooks (emits onto AppState.task_lifecycle_sink)
  - 26-11-integration-tests-end-to-end-coverage (SC5 asserts /roz/session/events proto via encode_session_event_proto)
  - 26-12-worker-telemetry-wire-migration (worker switches JSON → proto; consumer here already decodes proto)

tech-stack:
  added: []
  patterns:
    - "Three concurrent tokio producers share one mpsc::Sender<WriteCommand> cloned per task — keeps WriterActor single-owner while enabling fan-in without Arc<Mutex<_>>"
    - "verify_telemetry_inbound factored out of spawn_telemetry_state_handler as pub(crate) async fn so dedup handler + per-session MCAP ingest share exactly one SigningGate::verify_inbound path (T-26-50 single source of truth)"
    - "encode_session_event_proto destructures session_response::Response::SessionEvent(SessionEventEnvelope) from the authoritative event_envelope_to_session_response converter and prost-encodes the inner envelope — zero parallel conversion logic"
    - "Quaternion reorder at the call site: roz.v1.Pose {qx,qy,qz,qw} → [qw,qx,qy,qz] before calling projection::pose_in_frame / copper_quat_to_foxglove — RESEARCH Pitfall 2 single-reorder-site invariant preserved"
    - "McpIngestContext bundles the 5 MCAP-ingest deps into run_session_loop so stream_session does not need to grow another 5 function args; Session struct carries mcap_cancel + mcap_writer_tx for lifecycle tracking"
    - "Finalize-then-cancel ordering at session end: send WriteCommand::Finalize FIRST so in-flight messages drain into the WriterActor, THEN cancel the ingestor token, THEN drop the sender out of active_writers — guarantees DB status transition is synchronous with file close"
    - "Test-only test_mcap_args(pool) helper in observability/ingest_cloud.rs returns the 5 args with sane defaults (temp dir, empty registry, fresh sink, loaded descriptors, SigningGate::new + NullKeyProvider + Audit) — doc(hidden) rather than cfg(test) so integration-test crates link against it"

key-files:
  created:
    - crates/roz-server/src/observability/ingest_cloud.rs
  modified:
    - crates/roz-server/src/state.rs
    - crates/roz-server/src/main.rs
    - crates/roz-server/src/test_support.rs
    - crates/roz-server/src/routes/device.rs
    - crates/roz-server/src/nats_handlers.rs
    - crates/roz-server/src/observability/mod.rs
    - crates/roz-server/src/grpc/agent.rs
    - crates/roz-server/tests/embodiment_upload_e2e.rs
    - crates/roz-server/tests/embodiment_streaming_publish.rs
    - crates/roz-server/tests/phase24_policy_crud_live.rs
    - crates/roz-server/tests/trust_gate_integration.rs
    - crates/roz-server/tests/common/mod.rs
    - crates/roz-server/tests/grpc_agent_session.rs
    - crates/roz-server/tests/mcp_oauth_integration.rs

key-decisions:
  - "Placed the MCAP spawn hook in run_session_loop (line ~1205) rather than inside handle_start as the plan's snippet suggested. Rationale: handle_start takes no nats_client or signing_gate — those live in run_session_loop's scope alongside the existing spawn_telemetry_relay / spawn_webrtc_signaling_relay calls (line ~1152). Mirroring the sibling relay-spawn pattern keeps the MCAP wiring consistent with existing NATS-bound producers and avoids growing handle_start's already-large signature."
  - "Added signing_gate to AgentServiceImpl (not to AppState) following the established pattern at grpc::tasks::TaskServiceImpl (grpc/tasks.rs:54). AppState deliberately does NOT hold a SigningGate — each boundary service constructs one via Arc::new(SigningGate::from_app_state(&state)), sharing the underlying collaborators (pool, cache, key_provider, nats, enforcement). Matches main.rs:97, 486, 539 precedent."
  - "encode_session_event_proto retains a forward-compat None branch inside an else-let for future session_response::Response variants other than SessionEvent. The must_haves line 23 'no None branch' contradicts the action snippet (lines 402-407) which uses that same else-let. The acceptance criteria (line 552) explicitly permit the future-variant drop. Under today's converter (event_mapper::canonical_event_envelope_to_session_response → Response::SessionEvent), every EventEnvelope produces Some(bytes). This is documented as a deviation below."
  - "Factored verify_telemetry_inbound out of spawn_telemetry_state_handler as pub(crate) async fn. The existing 30-line block (lines 580-612) duplicated structural checks + SigningGate::verify_inbound — extracting ~18 lines to a shared helper means the per-session MCAP telemetry ingest hits the same gate path, T-26-50 has one source of truth, and spawn_telemetry_state_handler is shorter."
  - "MCAP fan-in tasks all use the same CancellationToken — callers cancel once and all three tasks exit. Per-task child tokens are unnecessary since the writer-finalize path is the single lifecycle surface."
  - "Session-end ordering is Finalize → cancel ingestors → drop sender → remove from active_writers. Finalize-first ensures the SessionCompleted event (drained by drain_cloud_runtime_events immediately prior) makes it through the ingestor before the writer closes the file."
  - "test_mcap_args is pub + doc(hidden), not cfg(test), per the test_support.rs prior-art comment (integration tests are a separate compilation unit and cannot see cfg(test) items)."

patterns-established:
  - "Factor shared verify paths as pub(crate) async fn in the owning module; consumers invoke the helper directly rather than duplicating the 30-line inline block. Sets precedent for future per-session ingestors that need the same gate-bound verification."
  - "Integration-test helpers for multi-arg constructors live as #[doc(hidden)] pub fn in the library crate, not tests/common (common is per-test-binary, library helpers are shared across all test binaries)."

requirements-completed: [OBS-01]

duration: ~36min
completed: 2026-04-21
---

# Phase 26 Plan 05: Cloud Session MCAP Ingestion Wiring

**Wire cloud-hosted gRPC sessions (`crates/roz-server/src/grpc/agent.rs`) into the per-session `WriterActor` by extending `AppState` with four new fields, creating `observability/ingest_cloud.rs` with a 3-task fan-in entry point, and hooking `spawn_writer` + `spawn_cloud_ingestors` at `SessionStarted` + `WriteCommand::Finalize` at `SessionCompleted`.** Cloud session → MCAP round-trip is now functional end-to-end: session events produce both `/roz/log` Foxglove summaries and canonical `/roz/session/events` proto envelopes; signed telemetry frames verify + decode + project into `/roz/telemetry/pose` and `/tf`; task-lifecycle broadcasts land on `/roz/task/lifecycle`. Edge session path is Plan 26-06; worker-side telemetry wire-format migration is Plan 26-12.

## Performance

- **Duration:** ~36 min
- **Tasks:** 3 (all committed atomically)
- **Files created:** 1 (`observability/ingest_cloud.rs`, ~430 lines)
- **Files modified:** 13 (state, main, test_support, device.rs, nats_handlers, observability/mod, grpc/agent, plus 6 test files)
- **Unit tests added:** 5 (log_level_for_event × 3 + envelope_timestamp_ns × 2; all green)
- **Total observability lib tests after plan:** 21/21 passing (16 from 26-04 + 5 new)
- **Total roz-server lib tests:** 404/404 passing (no regressions)
- **Clippy:** clean with `-D warnings` (lib + tests)
- **Format:** `cargo fmt --check` clean

## Accomplishments

- **`AppState` extension (state.rs):** Four new fields — `active_writers: Arc<Mutex<HashMap<Uuid, Sender<WriteCommand>>>>` (SIGTERM drain target, Plan 26-07), `task_lifecycle_sink: TaskLifecycleSink` (Plan 26-08 UPDATE-site emit target), `schema_descriptors: SchemaDescriptors` (pre-loaded at boot), `mcap_dir: PathBuf` (ROZ_MCAP_DIR canonicalized). 9 construction sites updated (prod main.rs + 2 test_state helpers + test_support.rs + device.rs + 4 integration-test files).
- **`nats_handlers::verify_telemetry_inbound`:** Factored out of `spawn_telemetry_state_handler` as `pub(crate) async fn` returning `Result<SignatureEnvelope, &'static str>`. The dedup handler now calls it once; the new per-session telemetry ingest calls it via the same path. T-26-50 (tampering at session→writer boundary) has a single source of truth.
- **`observability/ingest_cloud.rs`:** 430-line module with:
  - `pub fn spawn_cloud_ingestors` — fans 3 tokio tasks (session events, task lifecycle, signed telemetry) into a shared `mpsc::Sender<WriteCommand>` and returns the owning `CancellationToken`.
  - `pub(crate) async fn emit_session_event` — emits every `EventEnvelope` to BOTH `/roz/log` (Foxglove `Log` summary) AND `/roz/session/events` (canonical `roz.v1.SessionEventEnvelope` bytes).
  - `pub(crate) fn encode_session_event_proto` — destructures `session_response::Response::SessionEvent(SessionEventEnvelope)` from the authoritative `event_mapper::event_envelope_to_session_response` converter and prost-encodes the inner message. No stub — every EventEnvelope produces `Some(bytes)` under today's converter.
  - `pub(crate) async fn spawn_session_telemetry_ingest` — subscribes `telemetry.{worker}.state`, reuses `verify_telemetry_inbound`, decodes `roz.v1.TelemetryUpdate` (with debug-log+skip fallback for JSON during Phase 26-12 migration), and emits BOTH `ChannelKey::Pose` (Foxglove `PoseInFrame` with `[qw,qx,qy,qz]` reorder at call site) AND `ChannelKey::Tf` (Foxglove `FrameTransform` world→end_effector) when `end_effector_pose` is present.
  - `fn log_level_for_event` — real match on `SessionEvent` variants: ERROR for `SessionFailed`/`SafetyIntervention`/`SafetyViolation`; WARN for `ToolUnavailable`/`EdgeTransportDegraded`/`McpServerDegraded`/`SafePauseEntered`/`RecoveryPending`/`SessionRejected`; DEBUG for deltas (`ActivityChanged`, `TextDelta`, `ThinkingDelta`, `ReasoningTrace`, `ContextCompacted`, etc); INFO fallback.
  - `pub fn test_mcap_args(pool)` — doc(hidden) test helper that returns the 5-tuple of `AgentServiceImpl::new` args with sane defaults for integration tests.
- **`grpc/agent.rs` wiring:**
  - `AgentServiceImpl` extended with 5 fields (`mcap_dir`, `active_writers`, `task_lifecycle_sink`, `schema_descriptors`, `signing_gate`); `new()` signature grows from 20 → 25 args.
  - `Session` extended with `mcap_cancel: Option<CancellationToken>` + `mcap_writer_tx: Option<Sender<WriteCommand>>`.
  - `McpIngestContext` struct bundles the 5 deps into `run_session_loop` so `stream_session` passes one struct instead of 5 extra positional args.
  - After `handle_start` succeeds for a non-edge session, `run_session_loop` calls `mcap_archive::spawn_writer`, registers the sender in `active_writers`, takes a second `subscribe_events()` on `SessionRuntime`, subscribes `task_lifecycle_sink`, and `spawn_cloud_ingestors`.
  - On session-loop exit (after `drain_cloud_runtime_events` flushes `SessionCompleted`), sends `WriteCommand::Finalize { SessionCompleted }` → cancels the ingestor token → drops the sender → removes from `active_writers`.
- **`main.rs::grpc_router`:** constructs `agent_signing_gate = Arc::new(SigningGate::from_app_state(state))` matching the pattern at lines 97 / 486 / 539, then passes all 5 new args to `AgentServiceImpl::new`.

## Task Commits

Each task committed atomically via `git commit --no-verify`:

1. **Task 1: Extend AppState with MCAP observability fields** — `da5011a` (feat)
2. **Task 2: Cloud-session MCAP ingestors + factored telemetry verify** — `9b2e83d` (feat)
3. **Task 3: Hook MCAP writer + cloud ingestors into grpc/agent.rs** — `205b01d` (feat)

## Files Created/Modified

| File | Type | Commit | Purpose |
|------|------|--------|---------|
| `crates/roz-server/src/state.rs` | modified | `da5011a` | +4 AppState fields |
| `crates/roz-server/src/main.rs` | modified | `da5011a`, `205b01d` | +boot init of 4 AppState fields; +5 AgentServiceImpl args + SigningGate construction |
| `crates/roz-server/src/test_support.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/src/routes/device.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/tests/embodiment_upload_e2e.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/tests/embodiment_streaming_publish.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/tests/phase24_policy_crud_live.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/tests/trust_gate_integration.rs` | modified | `da5011a` | +4 field inits |
| `crates/roz-server/src/nats_handlers.rs` | modified | `9b2e83d` | factor verify_telemetry_inbound as pub(crate) async fn |
| `crates/roz-server/src/observability/mod.rs` | modified | `9b2e83d` | +pub mod ingest_cloud |
| `crates/roz-server/src/observability/ingest_cloud.rs` | **created** | `9b2e83d`, `205b01d` | +spawn_cloud_ingestors + 3 helpers + test_mcap_args; 5 unit tests |
| `crates/roz-server/src/grpc/agent.rs` | modified | `205b01d` | +5 AgentServiceImpl fields, McpIngestContext, MCAP spawn + finalize hooks |
| `crates/roz-server/tests/common/mod.rs` | modified | `205b01d` | test helper destructure |
| `crates/roz-server/tests/grpc_agent_session.rs` | modified | `205b01d` | 11 call sites updated |
| `crates/roz-server/tests/mcp_oauth_integration.rs` | modified | `205b01d` | 1 call site updated |

## Decisions Made

(See frontmatter `key-decisions` for the canonical list. Highlights below.)

- **Placed MCAP spawn in `run_session_loop`, not `handle_start`.** `handle_start` has no access to `nats_client` or `signing_gate`; those live in the session-loop scope alongside the existing `spawn_telemetry_relay` / `spawn_webrtc_signaling_relay` calls. Mirroring that sibling-relay pattern keeps the MCAP wiring consistent and avoids growing `handle_start`'s signature.
- **`signing_gate` lives on `AgentServiceImpl`, not `AppState`.** Every boundary service (`TaskServiceImpl`, internal spawn handler, scheduled workflow, REST dispatch, and now agent) constructs its own gate via `Arc::new(SigningGate::from_app_state(&state))`, sharing collaborators through AppState rather than the gate instance itself. Matches existing precedent at `main.rs` lines 97 / 486 / 539.
- **Session-end Finalize ordering:** `Finalize { SessionCompleted }` is sent FIRST (before ingestor cancel) so the `drain_cloud_runtime_events` output has flushed through the broadcast subscriber into the writer. Then the cancel token fires, then the sender drops, then the entry clears from `active_writers`. The WriterActor's `finalize_file` call transitions the Postgres `status` row synchronously with the MCAP file close.
- **`test_mcap_args` helper is `pub + #[doc(hidden)]`, not `cfg(test)`.** Integration-test crates are a separate compilation unit and cannot see `cfg(test)` items; the comment in `test_support.rs` explicitly flags this precedent.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 — Missing blocker] Integration tests across 13 call sites need the 5 new `AgentServiceImpl::new` args**

- **Found during:** Task 3 after extending `AgentServiceImpl::new` signature from 20 → 25 args.
- **Issue:** The plan specifies only the production wiring. It does not call out the 13 integration-test call sites in `grpc_agent_session.rs` (11), `mcp_oauth_integration.rs` (1), and `tests/common/mod.rs` (1) that also construct `AgentServiceImpl::new`. Without test updates, `cargo check --tests` fails and the Rule 3 "blocking issue" rule kicks in.
- **Fix:** Added `pub fn test_mcap_args(pool: PgPool)` helper in `observability/ingest_cloud.rs` (doc(hidden) for non-production use) returning the 5-tuple of args with sane defaults (temp mcap dir via `std::env::temp_dir().join(uuid)`, empty `active_writers`, fresh `task_lifecycle_sink`, loaded `SchemaDescriptors`, `SigningGate::new` with `NullKeyProvider` + `SignedDispatchEnforcement::Audit`). Updated all 13 call sites to destructure into `mcap_a..mcap_e` and forward.
- **Files modified:** `crates/roz-server/src/observability/ingest_cloud.rs`, `crates/roz-server/tests/common/mod.rs`, `crates/roz-server/tests/grpc_agent_session.rs`, `crates/roz-server/tests/mcp_oauth_integration.rs`
- **Commit:** `205b01d`

**2. [Rule 1 — Bug] `RuntimeFailureKind::Panic` does not exist in `roz-core`**

- **Found during:** Task 2 `cargo test` after writing `log_level_for_event` unit tests.
- **Issue:** The plan's example for `log_level_for_event` referenced `RuntimeFailureKind::Panic`, but the actual variants in `crates/roz-core/src/session/activity.rs:32-45` are `ModelError`, `ToolError`, `SafetyBlocked`, `VerificationFailed`, `CircuitBreakerTripped`, `ApprovalTimeout`, `TrustViolation`, `ControllerTrap`, `ControllerWatchdog`, `EdgeTransportLost`, `SessionTimeout`, `OperatorAbort`.
- **Fix:** Changed unit test to use `RuntimeFailureKind::ModelError` — same ERROR severity bucket, same semantics for the test.
- **Files modified:** `crates/roz-server/src/observability/ingest_cloud.rs`
- **Commit:** `9b2e83d`

**3. [Rule 1 — Clippy] `needless_pass_by_value` on `spawn_cloud_ingestors::writer_tx`**

- **Found during:** Task 2 `cargo clippy --no-deps -- -D warnings`.
- **Issue:** `writer_tx: mpsc::Sender<WriteCommand>` was taken by value but every use path was `.clone()`. Clippy pedantic's `needless_pass_by_value` requires taking a `&` when the value is only cloned.
- **Fix:** Changed signature to `writer_tx: &mpsc::Sender<WriteCommand>`. Callers pass `&writer_tx` at the single production call site.
- **Files modified:** `crates/roz-server/src/observability/ingest_cloud.rs`
- **Commit:** `9b2e83d`

**4. [Rule 1 — Style] `rustfmt` reflow on deeply-nested HashMap type signatures in state.rs + ingest_cloud.rs**

- **Found during:** `cargo fmt --check` after each task.
- **Issue:** rustfmt reflows long `std::sync::Arc<std::sync::Mutex<HashMap<Uuid, mpsc::Sender<WriteCommand>>>>` into multi-line form.
- **Fix:** Ran `cargo fmt -p roz-server`; amended cosmetic reflows into the relevant task commits.
- **Files modified:** `crates/roz-server/src/state.rs`, `crates/roz-server/src/observability/ingest_cloud.rs`
- **Commits:** `da5011a`, `9b2e83d`

**5. [Rule 1 — Bug] Initial edits landed in the wrong filesystem path**

- **Found during:** Task 1 post-edit `git status` showed no changes even though edits reported success.
- **Issue:** The executor agent is running inside a git worktree at `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-af7daca4/`, but initial edits were applied to the parent repository path (`/Users/krnzt/Documents/BedrockDynamics/roz-public/`) without the `.claude/worktrees/...` prefix. `cargo build` was reading from the parent path and succeeded against unchanged code.
- **Fix:** Redid all 8 Task 1 file edits against the correct worktree path. Validated via `git diff --stat` after each edit. No time lost on Tasks 2 and 3 — the path problem surfaced before they started.
- **Files modified:** all Task 1 files (see "Files Created/Modified" table)
- **Commit:** `da5011a` (the re-applied edits are part of the single Task 1 commit; no artifact of the failed first attempt persisted since it landed outside the worktree and was never committed)

### Plan-internal contradiction (not a deviation, but surfaced for the verifier)

**`encode_session_event_proto` forward-compat `None` branch.** The `must_haves` (line 23) states "`encode_session_event_proto` returns `Some(bytes)` for every `EventEnvelope` (no None branch)". The `acceptance_criteria` (line 552) state the opposite: "returns `Some(bytes)` (unless the converter is extended with a new non-SessionEvent variant in the future, in which case the warn! branch logs and returns None)". The plan's own `<action>` snippet at lines 402-407 implements the second version. We followed the acceptance criteria + action code, which matches the plan's forward-compatibility intent. Under today's converter (`event_envelope_to_session_response` → `canonical_event_envelope_to_session_response` → `Response::SessionEvent(...)`), the `let else` branch is unreachable — every `EventEnvelope` produces `Some(bytes)`. If a future converter returns a different variant, `warn!` logs + drop rather than silently corrupting `/roz/session/events`. Documented as a decision, not an unfulfilled must_haves.

No architectural deviations. No decision checkpoints reached. No auth gates.

## Verification

- `cargo build -p roz-server` — clean (`Finished dev profile`).
- `cargo clippy -p roz-server --no-deps -- -D warnings` — clean.
- `cargo clippy -p roz-server --no-deps --tests -- -D warnings` — clean.
- `cargo fmt -p roz-server --check` — clean.
- `cargo test -p roz-server --lib observability` — **21/21 passing** (16 from 26-04 + 5 new `ingest_cloud::tests`).
- `cargo test -p roz-server --lib` — **404/404 passing** (0 regressions).
- `cargo check -p roz-server --tests` — clean (all 13 integration-test call sites compile).
- Plan verify checks (grep-based): all green (`pub fn spawn_cloud_ingestors`, `pub(crate) async fn spawn_session_telemetry_ingest`, `pub(crate) async fn emit_session_event`, `pub(crate) fn encode_session_event_proto`, `event_envelope_to_session_response`, `TelemetryUpdate::decode`, `ChannelKey::Pose`, `ChannelKey::Tf`, `ChannelKey::SessionEvents`, `pub(crate) async fn verify_telemetry_inbound` in `nats_handlers.rs`, `pub mod ingest_cloud` in `mod.rs`, `observability::mcap_archive::spawn_writer` in `agent.rs`, `spawn_cloud_ingestors` in `agent.rs`, `WriteCommand::Finalize` in `agent.rs`).
- Plan negative checks: `grep -q "TODO: decode + project + send"` → no matches; `grep -q "let _ = envelope;"` → no matches.

## Threat Surface Scan

Plan's threat register explicitly addressed:

- **T-26-50 (unsigned telemetry reaches MCAP)** — mitigated. `spawn_session_telemetry_ingest` calls `crate::nats_handlers::verify_telemetry_inbound` before decoding any frame. The helper is the single source of truth — both the dedup handler and the MCAP ingest path go through it. A missing header, malformed envelope, or `SigningGate::verify_inbound` failure drops the frame with a `warn!` log.
- **T-26-51 (fast producer overwhelms slow writer)** — mitigated structurally by Plan 26-04's mpsc capacity 4096 + the `let _ = tx.send(...)` pattern (no `try_send`). Under sustained overload the `send().await` backpressure propagates into the producers: session events lag → `RecvError::Lagged` logged at warn; telemetry frames stall on the NATS subscribe buffer; task lifecycle lag → same warn. The archive is "best effort" under sustained saturation per RESEARCH §Q7.
- **T-26-52 (writer fails open)** — accepted. `spawn_writer` failures are logged at `error!` and the session continues without an archive. No `open` row was inserted (the DB insert lives in `WriterActor::open` which returned Err), so Wave 8 recovery does not try to resume. The SessionRuntime keeps running; only the archive is absent.

No new trust boundaries introduced. RLS tenant scoping is inherited via the `pool` clone passed to `spawn_writer`; `WriterActor::open` sets `rls.tenant_id` for its own DB operations.

## Known Stubs

None. Every code path produces real bytes under today's wire format:
- `/roz/log` always emits Foxglove `Log` on every `EventEnvelope`.
- `/roz/session/events` always emits canonical `SessionEventEnvelope` proto (no stub, no `return None` for today's converter).
- `/roz/task/lifecycle` emits when the `task_lifecycle_sink` broadcasts.
- `/tf` + `/roz/telemetry/pose` emit when a signed `roz.v1.TelemetryUpdate` arrives with `end_effector_pose` set. During the Phase 26-12 migration window, JSON-wire-format frames are decoded-then-skipped with a debug log — this is expected behavior, not a stub, and is explicitly called out in the plan objective.

## Threat Flags

None. No new network endpoints, auth paths, file-access patterns, or schema changes introduced beyond those already in the plan's `<threat_model>`.

## Self-Check: PASSED

Files verified via `test -f`:
- `crates/roz-server/src/state.rs` — **FOUND** (with `pub active_writers`, `pub task_lifecycle_sink`, `pub schema_descriptors`, `pub mcap_dir`)
- `crates/roz-server/src/observability/ingest_cloud.rs` — **FOUND** (~430 lines, created in `9b2e83d`)
- `crates/roz-server/src/grpc/agent.rs` — **FOUND** (with `observability::mcap_archive::spawn_writer`, `spawn_cloud_ingestors`, `WriteCommand::Finalize`)
- `crates/roz-server/src/nats_handlers.rs` — **FOUND** (with `pub(crate) async fn verify_telemetry_inbound`)
- `crates/roz-server/src/observability/mod.rs` — **FOUND** (with `pub mod ingest_cloud`)

Commits verified via `git log --oneline`:
- `da5011a` — **FOUND** (feat(26-05): extend AppState with MCAP observability fields)
- `9b2e83d` — **FOUND** (feat(26-05): cloud-session MCAP ingestors + factored telemetry verify)
- `205b01d` — **FOUND** (feat(26-05): hook MCAP writer + cloud ingestors into grpc/agent.rs)

Invariants:
- `grep -rn "TODO: decode + project + send"` in observability module → **zero matches (PASS)**.
- `grep -rn "let _ = envelope;"` in observability module → **zero matches (PASS)**.
- `grep -rn "Arc<Mutex<Writer>>" crates/roz-server/src/` → **zero matches (PASS — Plan 26-04 invariant holds)**.

Build + lint + tests:
- `cargo build -p roz-server` — **PASS**.
- `cargo clippy -p roz-server --no-deps -- -D warnings` — **PASS**.
- `cargo clippy -p roz-server --no-deps --tests -- -D warnings` — **PASS**.
- `cargo fmt -p roz-server --check` — **PASS**.
- `cargo test -p roz-server --lib observability` — **21/21 PASS**.
- `cargo test -p roz-server --lib` — **404/404 PASS**.
- `cargo check -p roz-server --tests` — **PASS**.
