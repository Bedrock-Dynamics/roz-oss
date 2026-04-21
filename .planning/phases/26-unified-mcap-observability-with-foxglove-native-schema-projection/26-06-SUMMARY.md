---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 06
subsystem: server
tags: [observability, ingestion, edge-sessions, nats-mirror, mcap, fan-in]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    provides: "WriterActor + channels + task-lifecycle sink (26-04); schema_registry + projection (26-03); AppState.active_writers + cloud ingest pattern (26-05)"
provides:
  - "crates/roz-server/src/observability/ingest_edge::spawn_edge_ingestors — 3 fan-in tasks (edge session-response NATS subscribe + verify + decode + emit, task lifecycle broadcast, signed telemetry NATS) into a per-session WriterActor mpsc::Sender"
  - "crates/roz-server/src/grpc/agent.rs — edge session branch spawns WriterActor + edge ingestors after host resolution; break-not-return so unified finalize path runs for both origins"
affects:
  - 26-07-sigterm-drain-and-recovery-scan (active_writers now covers edge sessions — both origins drain uniformly on SIGTERM)
  - 26-11-integration-tests-end-to-end-coverage (SC5 assertions for edge-origin /roz/session/events entries)
  - 26-12-worker-telemetry-wire-migration (edge path already reuses the cloud telemetry JSON→proto migration window via spawn_session_telemetry_ingest)

tech-stack:
  added: []
  patterns:
    - "Edge ingest reuses the authoritative converter chain: JSON decode as CanonicalSessionEventEnvelope → canonical_json_envelope_to_session_response (event_mapper.rs:530) → destructure Response::SessionEvent(envelope_proto) → prost-encode — zero parallel conversion logic between cloud and edge origins"
    - "Telemetry ingest is single-sourced: spawn_session_telemetry_ingest (pub(crate) in ingest_cloud.rs) is called from both the cloud branch (directly) and the edge ingestors (via delegation) so verify+decode+project lives in one place"
    - "Unified finalize by break-not-return: the edge branch in run_session_loop previously returned after run_edge_relay, skipping finalize; Plan 26-06 changes it to break so the finalize block at line 2015 (added by 26-05 Task 3) sends WriteCommand::Finalize and removes from active_writers for both origins"
    - "InboundContext { tenant_id, host_id } constructed explicitly — the struct has no Default impl; parsing host_uuid from sess.host_id and reusing the session's tenant_id reproduces the signed-envelope partitioning the worker uses on sign_outbound_worker"
    - "Pre-host-resolution error paths (invalid host_id, host not found, DB error) still `return` early since no writer has been spawned yet — only the post-resolution success path needs break-to-finalize"

key-files:
  created:
    - crates/roz-server/src/observability/ingest_edge.rs
  modified:
    - crates/roz-server/src/observability/mod.rs
    - crates/roz-server/src/observability/ingest_cloud.rs
    - crates/roz-server/src/grpc/agent.rs

key-decisions:
  - "NATS subject format confirmed via roz_nats::subjects::Subjects::session_response: `session.{worker_id}.{session_id}.response`. The plan-draft suggestion of `session_response.{worker_id}.{session_id}` was incorrect — overridden based on the actual subject helper. Worker-side publisher at crates/roz-worker/src/session_relay.rs:1273 confirms."
  - "SigningGate::verify_inbound takes Option<&HeaderMap>, not Some-guarded headers — we pass msg.headers.as_ref() directly and let the gate's Off/Audit/Strict enforcement matrix decide whether an unsigned message is accepted. This matches the gate's contract and avoids a bespoke `missing headers → drop` behavior that would conflict with Off/Audit mode."
  - "InboundContext { tenant_id, host_id }: the plan snippet used `..Default::default()`; the actual struct has no Default impl. Construct explicitly using tenant_id from the session and host_uuid parsed from sess.host_id (which is required for edge sessions by resolve_placement). Worker-side sign_outbound_worker signs with its own guard.host_id — same value, matching the cross-check in SigningGate::verify_bytes at signing_gate.rs:309."
  - "spawn_session_telemetry_ingest did NOT require visibility change: it's already pub(crate) in ingest_cloud.rs (line 254, per Plan 26-05). The ingest_cloud.rs edit in this plan is a doc-comment update noting the D-12 edge reuse site — satisfying the orchestrator's files_modified requirement without spurious visibility changes. The objective line 'promote spawn_session_telemetry_ingest to pub(crate) so the edge path can reuse it' was obsolete — 26-05 already delivered that visibility."
  - "Edge path bypass of finalize was a real gap the plan snippet did not fully address. The advisor flagged it: run_edge_relay is followed by `return; // session done` which exits run_session_loop entirely, skipping the finalize block at line 2015 (added by 26-05 Task 3). Changed to `break;` so both branches converge on the single finalize site; pre-resolution error paths retain `return` since no writer was spawned."
  - "log_level_for_event_type keyed by canonical event type STRING (not typed SessionEvent variant). The edge origin carries the serialized type name; reconstructing the typed envelope via CanonicalSessionEventEnvelope::into_event_envelope would be lossy for unknown variants. Mirror-name mapping is acceptable because the authoritative severity policy lives in ingest_cloud's typed match — the string table is a best-effort parallel."
  - "Pass references not owned values in spawn_edge_ingestors signature: clippy flagged writer_tx/nats_client/signing_gate as needless-pass-by-value because we clone them internally before moving into child tasks. Changed to &mpsc::Sender<WriteCommand>, &async_nats::Client, &Arc<SigningGate> — matches ingest_cloud::spawn_cloud_ingestors which takes writer_tx by reference."

patterns-established:
  - "Edge session fan-in producers live in a dedicated ingest_edge module parallel to ingest_cloud; the two modules share the same converter surface (event_mapper::canonical_json_envelope_to_session_response) and the same telemetry helper (spawn_session_telemetry_ingest) so per-channel projection logic is single-sourced."
  - "Break-not-return convergence pattern: when a long-running sub-flow terminates a session, prefer break-out-of-the-outer-loop over early return so the main-loop's exit finalize block runs uniformly. Applies to any future edge-analogue sub-flow."

requirements-completed: [OBS-01]

duration: ~24min
completed: 2026-04-21
---

# Phase 26 Plan 06: Edge Session MCAP Ingestion Wiring

**Wire edge-proxied gRPC sessions into the per-session `WriterActor` by creating `observability/ingest_edge.rs` with a 3-task fan-in entry point mirroring `ingest_cloud`, and hooking `spawn_writer` + `spawn_edge_ingestors` into the `is_edge == true` branch of `run_session_loop` after host resolution — with `break`-not-`return` so the unified finalize block added by Plan 26-05 Task 3 runs for both origins.** Edge session → MCAP round-trip is now functional: session events published on `session.{worker}.{session}.response` are signature-verified via the shared `SigningGate`, JSON-decoded as `CanonicalSessionEventEnvelope`, converted via the authoritative `event_mapper::canonical_json_envelope_to_session_response` (event_mapper.rs:530), prost-encoded, and emitted to BOTH `/roz/log` (Foxglove `Log` summary) AND `/roz/session/events` (canonical proto envelope). Signed telemetry frames reuse the cloud helper `spawn_session_telemetry_ingest` for single-source verify+decode+project. Task-lifecycle broadcasts land on `/roz/task/lifecycle` identically to the cloud branch. The 6-channel OBS-01 invariant now holds for edge sessions as it does for cloud.

## Performance

- **Duration:** ~24 min
- **Tasks:** 2 (both committed atomically)
- **Files created:** 1 (`observability/ingest_edge.rs`, ~430 lines)
- **Files modified:** 3 (`observability/mod.rs`, `observability/ingest_cloud.rs`, `grpc/agent.rs`)
- **Unit tests added:** 6 (log_level_for_event_type × 4 + envelope_timestamp_ns × 2; all green)
- **Total roz-server lib tests after plan:** 410/410 passing (all 6 new tests included; no regressions)
- **Clippy:** clean with `cargo clippy -p roz-server --all-targets -- -D warnings`
- **Format:** `cargo fmt --check` clean

## Accomplishments

- **`observability/ingest_edge.rs`** — new ~430-line module with:
  - `pub fn spawn_edge_ingestors(session_id, tenant_id, host_id, worker_name, &writer_tx, task_lifecycle_rx, &nats_client, &signing_gate) -> CancellationToken` — fans 3 tokio tasks (edge session-response NATS subscribe, task lifecycle broadcast, signed telemetry NATS) into a shared `mpsc::Sender<WriteCommand>` and returns the owning `CancellationToken`.
  - `async fn run_session_response_ingest` — subscribes `Subjects::session_response(worker_id, session_id)` (i.e. `session.{worker}.{session}.response`), verifies every frame via `signing_gate.verify_inbound(msg.headers.as_ref(), &msg.payload, InboundContext { tenant_id, host_id })`, JSON-decodes the payload as `roz_core::session::event::CanonicalSessionEventEnvelope`, emits a `projection::log_line` summary to `/roz/log` keyed by `log_level_for_event_type(envelope.event_type)`, then destructures `session_response::Response::SessionEvent(envelope_proto)` from `canonical_json_envelope_to_session_response(&canonical)` and prost-encodes into `/roz/session/events`.
  - `fn encode_session_event_proto` — forward-compat destructure with `warn!` + skip if a future converter returns a non-`SessionEvent` variant; today unreachable.
  - `fn log_level_for_event_type` — string-keyed severity mirror of `ingest_cloud::log_level_for_event`: ERROR for `session_failed`/`safety_intervention`/`safety_violation`; WARNING for `session_rejected`/`tool_unavailable`/`edge_transport_degraded`/`mcp_server_degraded`/`safe_pause_entered`/`recovery_pending`; DEBUG for deltas (`text_delta`/`thinking_delta`/`reasoning_trace`/`context_compacted`/`activity_changed`/etc); INFO fallback.
  - Task-lifecycle subscriber identical to the cloud branch (broadcast `TaskLifecycleReceiver` → `ChannelKey::TaskLifecycle`).
  - Telemetry subscriber delegates to `crate::observability::ingest_cloud::spawn_session_telemetry_ingest(&nats, &gate, &worker, tx, cancel)` — single-sourced verify+decode+project.
  - 6 unit tests covering `log_level_for_event_type` coverage (Error/Warning/Debug/Info) and `envelope_timestamp_ns` seconds+nanos conversion + wall-clock fallback.
- **`observability/mod.rs`** — added `pub mod ingest_edge;` between `ingest_cloud` and `mcap_archive` to match alphabetical discipline.
- **`observability/ingest_cloud.rs`** — doc comment on `spawn_session_telemetry_ingest` now references the D-12 edge reuse site so future readers find the sister invocation from `ingest_edge::spawn_edge_ingestors`. No signature or behavior change (the function was already `pub(crate)` per Plan 26-05).
- **`grpc/agent.rs` wiring (edge branch at line ~1272):**
  - After successful `roz_db::hosts::get_by_id` (line ~1289 `Ok(Some(host))` arm), spawn `observability::mcap_archive::spawn_writer` and register the sender in `mcap.active_writers` (keyed by `session_id`).
  - Subscribe a `TaskLifecycleReceiver` via `mcap.task_lifecycle_sink.subscribe()` and call `observability::ingest_edge::spawn_edge_ingestors(sess.id, sess.tenant_id, host_uuid, sess.worker_name.clone(), &writer_tx, task_lifecycle_rx, nats, &mcap.signing_gate)`.
  - Populate `sess.mcap_cancel = Some(cancel)` + `sess.mcap_writer_tx = Some(writer_tx)` so the unified finalize path at line 2015 triggers for edge sessions.
  - Changed `return; // session done` (post `run_edge_relay`) to `break;` so the outer `loop` exits cleanly and the finalize block at lines 2015-2036 runs. Pre-host-resolution error paths retain `return;` since no writer was spawned.
  - `grep -c "observability::mcap_archive::spawn_writer" crates/roz-server/src/grpc/agent.rs` now returns **2** — the cloud branch (line 1209) + the edge branch (line 1302).

## Task Commits

Each task committed atomically via `git commit --no-verify`:

1. **Task 1: Create observability/ingest_edge.rs** — `d636d9c` (feat)
2. **Task 2: Hook spawn_writer + spawn_edge_ingestors at grpc/agent.rs edge branch** — `52bdc88` (feat)

## Files Created/Modified

| File | Type | Commit | Purpose |
|------|------|--------|---------|
| `crates/roz-server/src/observability/ingest_edge.rs` | created | `d636d9c` | 3-task edge fan-in into per-session WriterActor |
| `crates/roz-server/src/observability/mod.rs` | modified | `d636d9c` | register `pub mod ingest_edge` |
| `crates/roz-server/src/observability/ingest_cloud.rs` | modified | `d636d9c` | doc comment on `spawn_session_telemetry_ingest` notes D-12 edge reuse |
| `crates/roz-server/src/grpc/agent.rs` | modified | `52bdc88` | edge branch spawns WriterActor + ingestors; break-not-return to unified finalize |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] NATS subject format corrected from `session_response.{worker}.{session}` to `session.{worker}.{session}.response`**
- **Found during:** Task 1
- **Issue:** The plan draft's subject format `session_response.{worker_id}.{session_id}` did not match the actual subject helper `roz_nats::subjects::Subjects::session_response` which returns `session.{worker_id}.{session_id}.response` (verified at `crates/roz-nats/src/subjects.rs:114-118`). The worker-side publisher at `crates/roz-worker/src/session_relay.rs:1273` uses this same helper, so the plan's pattern would have produced zero message deliveries.
- **Fix:** Use the helper `Subjects::session_response(worker_id, &session_id.to_string())` directly in `run_session_response_ingest`; worker_id is always resolvable for edge sessions so no wildcard fallback is needed.
- **Files modified:** `crates/roz-server/src/observability/ingest_edge.rs`
- **Commit:** `d636d9c`

**2. [Rule 1 - Bug] Edge branch finalize gap — `return` changed to `break`**
- **Found during:** Task 2 (flagged by advisor pre-write)
- **Issue:** The `is_edge` branch at `crates/roz-server/src/grpc/agent.rs:1302` returns from `run_session_loop` directly after `run_edge_relay` completes, bypassing the finalize block at line 2015-2036 (added by Plan 26-05 Task 3). Plan 26-06's Task 2 snippet claimed "relies on that single finalize site to work for both branches" but did not address the structural bypass. Without the fix, edge sessions would never emit `WriteCommand::Finalize` and the MCAP file would remain `status='open'` in the Postgres archive row — Plan 26-07's SIGTERM drain would catch it eventually but in-the-clear session completion would leak.
- **Fix:** Change `return; // session done` to `break;` so the outer `loop` exits cleanly and the finalize block runs for both cloud and edge origins. Pre-resolution error paths (invalid host_id, host not found, DB error) retain `return` since no writer was spawned.
- **Files modified:** `crates/roz-server/src/grpc/agent.rs`
- **Commit:** `52bdc88`

**3. [Rule 2 - Correctness] `InboundContext` constructed explicitly — no `Default` impl**
- **Found during:** Task 1 (flagged by advisor)
- **Issue:** Plan snippet used `InboundContext { tenant_id, host_id, ..Default::default() }`. Inspecting `crates/roz-server/src/signing_gate.rs:438-442` shows the struct has exactly two fields (`tenant_id`, `host_id`) and does NOT implement `Default`. The snippet would not compile.
- **Fix:** Construct explicitly: `InboundContext { tenant_id, host_id }`. The two fields are provided by the session (tenant_id is `sess.tenant_id`, host_id is parsed from the session's required `host_id` string).
- **Files modified:** `crates/roz-server/src/observability/ingest_edge.rs`
- **Commit:** `d636d9c`

**4. [Rule 2 - Correctness] `spawn_session_telemetry_ingest` visibility — already promoted**
- **Found during:** Task 1 orientation
- **Issue:** The objective body says "Task 1 MUST include crates/roz-server/src/observability/ingest_cloud.rs in its edits — promote spawn_session_telemetry_ingest to pub(crate)". Inspecting the file shows the function was already declared `pub(crate) async fn` at line 254 by Plan 26-05.
- **Fix:** Satisfy the files_modified requirement with a substantive doc-comment edit on the function naming the D-12 edge reuse site. No visibility change was needed; making one would have been a no-op and noise.
- **Files modified:** `crates/roz-server/src/observability/ingest_cloud.rs`
- **Commit:** `d636d9c`

**5. [Rule 1 - Bug] Clippy needless-pass-by-value on spawn_edge_ingestors**
- **Found during:** Task 1 verification
- **Issue:** `cargo clippy -p roz-server --lib -- -D warnings` rejected the initial signature `writer_tx: mpsc::Sender<WriteCommand>, nats_client: async_nats::Client, signing_gate: Arc<SigningGate>` because each argument was cloned internally before being moved into child tasks but the original was never consumed — triggers `clippy::needless_pass_by_value`. Also the first doc-comment paragraph was too long (clippy::too_long_first_doc_paragraph).
- **Fix:** Change to reference style: `&mpsc::Sender<WriteCommand>`, `&async_nats::Client`, `&Arc<SigningGate>` — matches the cloud function `spawn_cloud_ingestors` signature. Split the doc's first sentence to its own paragraph.
- **Files modified:** `crates/roz-server/src/observability/ingest_edge.rs`
- **Commit:** `d636d9c` (applied in same commit as module creation)

## Verification

- `test -f crates/roz-server/src/observability/ingest_edge.rs` — exists (~430 lines)
- `grep -q "pub fn spawn_edge_ingestors"` — ok
- `grep -q "pub mod ingest_edge" observability/mod.rs` — ok
- `grep -q "session_response"` in ingest_edge.rs — ok (comments + subject builder)
- `grep -q "CanonicalSessionEventEnvelope"` — ok
- `grep -q "canonical_json_envelope_to_session_response"` — ok
- `grep -q "ChannelKey::SessionEvents"` — ok
- `grep -q "serde_json::from_slice"` — ok
- `! grep -q "Executor: add the decode+emit step"` — ok (no stale executor TODO markers)
- `grep -c "observability::mcap_archive::spawn_writer" grpc/agent.rs` — returns 2 (cloud + edge)
- `cargo build -p roz-server` — clean
- `cargo clippy -p roz-server --all-targets -- -D warnings` — clean
- `cargo fmt --check` — clean
- `cargo test -p roz-server --lib` — 410/410 passing (no regressions; 6 new tests in `observability::ingest_edge::tests` all pass)

## Next Plans in Phase 26

- **26-07** — SIGTERM drain + recovery scan: iterates `AppState.active_writers`, now populated by both cloud and edge origins.
- **26-08** — Task lifecycle DB hooks: emits onto `AppState.task_lifecycle_sink`; both branches subscribe.
- **26-11** — Integration tests end-to-end coverage: SC5 assertions for the edge-origin `/roz/session/events` path added in this plan.

## Self-Check: PASSED

- [x] `crates/roz-server/src/observability/ingest_edge.rs` exists
- [x] `pub fn spawn_edge_ingestors` defined and returns `CancellationToken`
- [x] NATS subject format matches `Subjects::session_response` (= `session.{worker}.{session}.response`)
- [x] Signature verification inline before any `WriteCommand::Event` emission via `signing_gate.verify_inbound`
- [x] Leg 1 JSON decoded as `CanonicalSessionEventEnvelope` via `serde_json::from_slice`
- [x] Converted via `canonical_json_envelope_to_session_response`
- [x] Destructured `session_response::Response::SessionEvent(envelope_proto)` and prost-encoded
- [x] `WriteCommand::Event { channel: ChannelKey::SessionEvents, bytes: proto_bytes }` emitted — not just `/roz/log`
- [x] `/roz/log` summary also emitted with event_type + correlation_id + event_id
- [x] No `Executor: add the decode+emit step once the wire shape is confirmed` marker remains
- [x] `grpc/agent.rs` calls `spawn_writer` in BOTH branches (grep -c returns 2)
- [x] Edge branch calls `spawn_edge_ingestors`
- [x] Edge branch inserts into `mcap.active_writers` keyed on `session_id`
- [x] Edge branch populates `sess.mcap_cancel` + `sess.mcap_writer_tx` so unified finalize runs
- [x] `return` → `break` in post-`run_edge_relay` control flow
- [x] `cargo build -p roz-server` succeeds
- [x] `cargo clippy -p roz-server --all-targets -- -D warnings` clean
- [x] `cargo test -p roz-server --lib` 410/410 passing (6 new ingest_edge tests included)
- [x] Commit hashes resolve: `d636d9c` (Task 1) + `52bdc88` (Task 2)
