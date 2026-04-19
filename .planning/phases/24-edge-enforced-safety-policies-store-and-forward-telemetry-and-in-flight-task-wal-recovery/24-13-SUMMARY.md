---
phase: 24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery
plan: 13
subsystem: roz-agent + roz-worker
tags: [agent-loop, checkpoint-triggers, session-event-violation, threading, wave-3, gap-closure]
requirements: [FS-01, FS-03]
gap_closure: true
dependency_graph:
  requires:
    - "crates/roz-worker/src/checkpoint_writer.rs::CheckpointTrigger + checkpoint_writer_channel (shipped in 24-04)"
    - "crates/roz-worker/src/dispatch.rs::emit_violation_event + enforcement_error_kind + severity_for_action (shipped in 24-05)"
    - "crates/roz-core/src/session/event.rs::SessionEvent::SafetyViolation + EventEnvelope (pre-Phase-24)"
    - "crates/roz-worker/src/main.rs::task_ckpt_tx + per-task CheckpointWriter (shipped in 24-12 Task 5)"
    - "crates/roz-worker/src/main.rs::session_event_tx broadcast (hoisted in 24-12 Task 4)"
  provides:
    - "roz_core::checkpoint_signal::CheckpointSignal trait (3 methods: tool_call_started, tool_call_completed, approval_received)"
    - "roz_core::checkpoint_signal::NoopCheckpointSignal fallback impl"
    - "roz_worker::checkpoint_writer::ChannelCheckpointSignal adapter wrapping mpsc::Sender<CheckpointTrigger>"
    - "AgentLoop field checkpoint_signal: Arc<dyn CheckpointSignal> + builder with_checkpoint_signal"
    - "AgentLoop emits tool_call_started / tool_call_completed / approval_received at the three D-08 locked transitions"
    - "execute_task signature extended with session_event_tx: broadcast::Sender<EventEnvelope>"
    - "Per-task mpsc→broadcast forwarder inside execute_task that wraps SessionEvent in EventEnvelope"
    - "Pre-dispatch gate Reject / Halt / Clamp branches emit SessionEvent::SafetyViolation via emit_violation_event AFTER write_safety_audit"
  affects:
    - "crates/roz-core/src/checkpoint_signal.rs (NEW)"
    - "crates/roz-core/src/lib.rs"
    - "crates/roz-worker/src/checkpoint_writer.rs"
    - "crates/roz-agent/src/agent_loop/mod.rs"
    - "crates/roz-agent/src/agent_loop/dispatch.rs"
    - "crates/roz-agent/src/agent_loop/approvals.rs"
    - "crates/roz-agent/src/agent_loop/core.rs"
    - "crates/roz-agent/tests/agent_loop.rs (tests only)"
    - "crates/roz-worker/src/main.rs"
    - "crates/roz-worker/src/dispatch.rs (tests only + 3 call-site assertions)"
tech-stack:
  added: []
  patterns:
    - "Narrow-trait-in-roz-core pattern — when a higher-tier crate (roz-agent) needs to signal into a lower-tier crate (roz-worker) without inverting the dependency graph, define the trait in the shared base crate (roz-core). roz-worker then provides a concrete `Channel*` adapter that implements the trait against its internal channel/state, while roz-agent consumes the trait through an Arc<dyn Trait>. Used here for CheckpointSignal; pattern is reusable for any future agent→worker callback surface."
    - "Additive builder with Noop default — AgentLoop::with_checkpoint_signal is additive. The constructor seeds Arc::new(NoopCheckpointSignal) so callers that don't opt in see zero behavior change. Existing 64 agent_loop integration tests pass unchanged. This preserves backward compatibility for CLI runs, local runtime, and non-Phase-24 cloud sessions."
    - "mpsc→broadcast forwarder for bridging channel kinds — when an existing helper function's signature (emit_violation_event takes mpsc::Sender) needs to route into a broadcast channel (session_event_tx) without breaking existing tests, spawn a per-task tokio task that drains the mpsc, wraps each event in EventEnvelope, and publishes on the broadcast. Error handling: best-effort try_send on mpsc, best-effort send on broadcast. Both match the established 24-12 RecoveryPending emit pattern."
    - "ApprovalReceived emitted only on approved-resolve path — the variant doc string in checkpoint_writer.rs explicitly says 'permission approval landed; physical-action gate cleared'. Denials and timeouts do not clear the gate; they are already captured via the tool-call error return. Emitting on all resolutions would double-count and dilute the semantic meaning of the trigger."
key-files:
  created:
    - crates/roz-core/src/checkpoint_signal.rs
  modified:
    - crates/roz-core/src/lib.rs
    - crates/roz-worker/src/checkpoint_writer.rs
    - crates/roz-agent/src/agent_loop/mod.rs
    - crates/roz-agent/src/agent_loop/dispatch.rs
    - crates/roz-agent/src/agent_loop/approvals.rs
    - crates/roz-agent/src/agent_loop/core.rs
    - crates/roz-agent/tests/agent_loop.rs
    - crates/roz-worker/src/main.rs
    - crates/roz-worker/src/dispatch.rs
decisions:
  - "Place CheckpointSignal trait in roz-core (not roz-agent) to avoid a dependency inversion: roz-agent already depends on roz-core and so does roz-worker, so the trait is visible to both without introducing agent→worker or worker→agent coupling."
  - "Emit ApprovalReceived ONLY on the approved-resolve branch of wait_for_human_approval. Denials and timeouts are not 'approval received' events per the D-08 variant doc. Test 4 (agent_loop_does_not_emit_approval_received_on_denial) guards against regressions."
  - "Use an mpsc→broadcast forwarder (Path B in plan) rather than changing emit_violation_event's signature to accept broadcast::Sender directly. Rationale: existing emit_violation_event signature is unit-tested, the broadcast type would cascade into Wave-2 24-12 RecoveryPending emission tests, and the mpsc→broadcast bridge is a single tokio::spawn block that cleanly bounds the divergence to execute_task's scope."
  - "Thread step_counter=i64::from(cycles) from run_streaming_core into dispatch_tool_calls and wait_for_human_approval. The counter matches the existing 'cycles' variable already tracked by the agent loop. Adding a step_counter parameter rather than a context struct keeps the churn to three function signatures and avoids rippling Phase-24-only context into non-Phase-24 callers."
  - "Remove the feature-gated drop(session_event_tx) that was used to silence unused-variable warnings under zenoh-off builds. After Task 3 the broadcast sender is cloned per-task in the subscribe loop (task_session_event_tx); the original session_event_tx binding is now consumed by every task spawn, so it naturally stays alive without the explicit drop."
  - "Tests for Task 2 live in crates/roz-agent/tests/agent_loop.rs (integration-test crate), reusing the existing setup_agent_loop fixture + RequireHumanApproval guard + resolve_approval helper. Task 3 tests live in crates/roz-worker/src/dispatch.rs's cfg(test) mod tests, reusing the existing policy_with_linear_limit + sample_invocation fixtures. No new test crate or harness required."
metrics:
  duration: "~1h 10min"
  completed: "2026-04-18"
  tasks_completed: 3
  commits: 3
  files_created: 1
  files_modified: 9
---

# Phase 24 Plan 13: Agent-loop checkpoint triggers + SafetyViolation emission Summary

**One-liner:** Closed VERIFICATION.md gaps 3 and 8-remaining by (a) threading a narrow `CheckpointSignal` trait through AgentLoop so it emits the three D-08 locked triggers (`ToolCallStarted`, `ToolCallCompleted`, `ApprovalReceived`) into the per-task `CheckpointWriter` wired in 24-12, and (b) calling `emit_violation_event` on every Reject / Halt / Clamp pre-dispatch outcome after `write_safety_audit` so operators see `SessionEvent::SafetyViolation` on the session event stream.

## Outcome

Before this plan, Phase 24 shipped:

- `CheckpointTrigger::ToolCallStarted` / `ToolCallCompleted` / `ApprovalReceived` variants with 0 production emitters (ripgrep confirmed).
- `emit_violation_event` helper with 0 production call sites.
- Per-task `CheckpointWriter` with a real `periodic_task_id` (24-12 Task 5) but only the 5 s periodic tick fired — the three event-driven triggers were silent.
- A per-task `task_ckpt_tx` held live as `_task_ckpt_sender_hold` in `execute_task`, waiting for 24-13 to plug into the agent loop.

After this plan:

- `roz_core::checkpoint_signal::CheckpointSignal` is the narrow trait the AgentLoop consumes; `ChannelCheckpointSignal` is the worker-side adapter forwarding trait calls onto `mpsc::Sender<CheckpointTrigger>` via `try_send`.
- `AgentLoop` has a `checkpoint_signal: Arc<dyn CheckpointSignal>` field defaulting to `NoopCheckpointSignal`, swappable via the additive builder `with_checkpoint_signal`.
- `AgentLoop::dispatch_tool_calls` emits `ToolCallStarted` / `ToolCallCompleted` for every pure-segment and physical-branch tool dispatch, on both success and error return paths.
- `AgentLoop::wait_for_human_approval` emits `ApprovalReceived` ONLY on the approved-resolve path (denials/timeouts intentionally do not fire per the D-08 variant semantics).
- `execute_task` receives `session_event_tx: broadcast::Sender<EventEnvelope>`, constructs a per-task mpsc→broadcast forwarder (`session_mpsc_tx`/`session_mpsc_rx`), and calls `emit_violation_event` on every non-Allow pre-dispatch branch after `write_safety_audit`.
- Pre-dispatch `PreDispatchOutcome::Reject(e)` → emit `SafetyViolation { kind: enforcement_error_kind(&e), action: "reject", severity: warning }`.
- Pre-dispatch `PreDispatchOutcome::Halt(e)` → emit `SafetyViolation { kind: enforcement_error_kind(&e), action: "halt", severity: error }`.
- Pre-dispatch `PreDispatchOutcome::Clamp { clamped_details }` → emit `SafetyViolation { kind: "limit_exceeded", action: "clamp", severity: warning, details: clamped_details }`.

## Commits

| # | Commit | Task | Files |
|---|--------|------|-------|
| 1 | `dc93cbf` | Task 1 — CheckpointSignal trait + ChannelCheckpointSignal adapter | crates/roz-core/src/checkpoint_signal.rs (new), crates/roz-core/src/lib.rs, crates/roz-worker/src/checkpoint_writer.rs |
| 2 | `62b5073` | Task 2 — AgentLoop field + builder + emission at three transitions | crates/roz-agent/src/agent_loop/{mod.rs, dispatch.rs, approvals.rs, core.rs}, crates/roz-agent/tests/agent_loop.rs |
| 3 | `86e7cf6` | Task 3 — execute_task wiring + pre-dispatch SafetyViolation emission | crates/roz-worker/src/main.rs, crates/roz-worker/src/dispatch.rs, crates/roz-agent/src/agent_loop/mod.rs (fmt) |

## What Changed

### Task 1 — CheckpointSignal trait + ChannelCheckpointSignal adapter

`crates/roz-core/src/checkpoint_signal.rs` (new):
- `pub trait CheckpointSignal: Send + Sync` with three fire-and-forget methods: `tool_call_started(&self, task_id, step_counter, call_id)`, `tool_call_completed(..)`, `approval_received(..)`.
- `pub struct NoopCheckpointSignal` — no-op impl for tests and non-Phase-24 environments.
- 2 paired unit tests (`noop_signal_is_a_valid_checkpoint_signal`, `noop_signal_is_send_sync`).

`crates/roz-core/src/lib.rs`:
- `pub mod checkpoint_signal;` registered alongside the existing `channels`/`clock` module declarations.

`crates/roz-worker/src/checkpoint_writer.rs`:
- `pub struct ChannelCheckpointSignal { tx: mpsc::Sender<CheckpointTrigger> }` + `ChannelCheckpointSignal::new(tx)` const constructor.
- `impl roz_core::checkpoint_signal::CheckpointSignal for ChannelCheckpointSignal` — each method constructs the matching `CheckpointTrigger::*` variant and calls `self.tx.try_send(...)`, discarding send errors (best-effort, matching existing `emit_violation_event`/`pre_dispatch_check` pattern).
- 5 new tests: `channel_signal_sends_tool_call_started_over_mpsc`, `..._completed_over_mpsc`, `..._approval_received_over_mpsc`, `channel_signal_swallows_send_errors_on_closed_receiver`, `channel_signal_is_send_sync`.

### Task 2 — AgentLoop emits triggers at D-08 locked transitions

`crates/roz-agent/src/agent_loop/mod.rs`:
- New field `checkpoint_signal: std::sync::Arc<dyn CheckpointSignal>` on `pub struct AgentLoop`.
- `AgentLoop::new` seeds it with `Arc::new(NoopCheckpointSignal)`; no change to the existing public constructor signature.
- New additive builder method `#[must_use] pub fn with_checkpoint_signal(mut self, signal: Arc<dyn CheckpointSignal>) -> Self`.

`crates/roz-agent/src/agent_loop/dispatch.rs`:
- `dispatch_tool_calls` gains an `i64 step_counter` parameter (threaded from `core.rs`).
- Pure-tool segment: `self.checkpoint_signal.tool_call_started(&tool_ctx.task_id, step_counter, &c.id)` before each `self.dispatcher.dispatch`; `tool_call_completed` on the matching `(idx, call_id, res)` tuple return.
- Physical-tool branch: `tool_call_started` fires BEFORE safety evaluation so the checkpoint captures intent even if the safety stack blocks. `tool_call_completed` fires on every return path (Approved, Blocked, NeedsHuman→resolved).

`crates/roz-agent/src/agent_loop/approvals.rs`:
- `wait_for_human_approval` gains `step_counter: i64`. On `ApprovalGateResult::Approved(effective_call)` the loop emits `self.checkpoint_signal.approval_received(&tool_ctx.task_id, step_counter, &call.id)` BEFORE `self.dispatcher.dispatch(&effective_call, ...)`.
- `ApprovalGateResult::Rejected` does NOT emit `approval_received` — the denial is captured via the returned `ToolResult::error("Permission denied by user for: ...")` which in turn triggers the `tool_call_completed` emission from the dispatch path.

`crates/roz-agent/src/agent_loop/core.rs`:
- Single-line change in the `run_streaming_core` dispatch invocation: `dispatch_tool_calls(..., i64::from(cycles))` threads the turn counter into the trigger stamp.

`crates/roz-agent/tests/agent_loop.rs`:
- New `mod checkpoint_signal_tests` containing:
  - `CountingCheckpointSignal` test helper (three `AtomicUsize` counters + three `Mutex<Option<(String,i64,String)>>` latest-arg slots).
  - `agent_loop_emits_tool_call_started_on_dispatch` — asserts count==1, task_id, call_id=="toolu_1", step_counter==1.
  - `agent_loop_emits_tool_call_completed_on_dispatch_return` — asserts started==completed==1.
  - `agent_loop_emits_approval_received_on_resolve` — uses existing `RequireHumanApproval` guard + `resolve_approval` helper, asserts exactly 1 approval received.
  - `agent_loop_does_not_emit_approval_received_on_denial` — same helpers with `approved=false`, asserts 0 approvals received.
  - `agent_loop_with_noop_signal_is_backward_compatible` — asserts default constructor path still produces `output.cycles == 2`.
- All 64 existing agent_loop integration tests continue to pass unchanged (confirmed via `cargo test -p roz-agent --test agent_loop`: 69 passed, 0 failed).

### Task 3 — execute_task wiring + pre-dispatch SafetyViolation emission

`crates/roz-worker/src/main.rs`:
- `execute_task` signature gains `session_event_tx: tokio::sync::broadcast::Sender<roz_core::session::event::EventEnvelope>`.
- Call site (worker subscribe loop) clones the module-level `session_event_tx` into `task_session_event_tx` alongside the existing per-task policy/wal/backpressure clones.
- The feature-gated `drop(session_event_tx)` under `#[cfg(not(feature = "zenoh"))]` is removed — per-task clones keep the broadcast alive for the worker lifetime.
- Near the `task_ckpt_tx` block (line 502–509), the sender is cloned into `task_ckpt_tx_for_agent`; the original binding is held in `_task_ckpt_sender_hold` exactly as Plan 24-12 Task 5 established.
- New per-task mpsc→broadcast forwarder tokio::spawn immediately after the checkpoint setup:
  ```rust
  let (session_mpsc_tx, mut session_mpsc_rx) =
      tokio::sync::mpsc::channel::<SessionEvent>(64);
  tokio::spawn(async move {
      while let Some(event) = session_mpsc_rx.recv().await {
          let envelope = EventEnvelope { event_id: EventId::new(), correlation_id: CorrelationId::new(), parent_event_id: None, timestamp: Utc::now(), event };
          let _ = forwarder_session_tx.send(envelope);
      }
  });
  ```
- Pre-dispatch `Clamp` branch (main.rs:~769) gets `emit_violation_event(&session_mpsc_tx, decision.policy_id, "limit_exceeded", "clamp", clamped_details)` after the audit write (`clamped_details.clone()` because `emit_violation_event` takes JSON by value and the audit also needs it).
- Pre-dispatch `Reject`/`Halt` branch (main.rs:~819) gets `emit_violation_event(&session_mpsc_tx, decision.policy_id, violation_kind, enforcement_action, details.clone())` after the audit write and BEFORE the early-return that signals SafetyStop to Restate.
- AgentLoop construction site (main.rs:~1096) now chains `.with_checkpoint_signal(Arc::new(ChannelCheckpointSignal::new(task_ckpt_tx_for_agent)))` onto the existing `.with_extensions(..).with_approval_runtime(..).with_turn_emitter_opt(..)` builder chain.

`crates/roz-worker/src/dispatch.rs`:
- 3 new unit tests in `#[cfg(test)] mod tests` (no changes to production code in this file):
  - `emit_violation_event_after_reject_decision_lands_on_session_stream`
  - `emit_violation_event_after_halt_decision_lands_on_session_stream`
  - `emit_violation_event_after_clamp_decision_lands_on_session_stream`
- Each test primes a `PolicyCache` + `HotPolicy` that produces the target outcome, calls `pre_dispatch_check` with an over-limit velocity, routes the result through `emit_violation_event` against a fresh mpsc pair, and asserts the received `SessionEvent::SafetyViolation` carries the expected `policy_id`, `violation_kind`, `enforcement_action`, `details`, and `severity_for_action` mapping.

## Acceptance Criteria Verification

| Task | Criterion | Status |
|------|-----------|--------|
| 1 | `grep -n "pub trait CheckpointSignal" crates/roz-core/src/checkpoint_signal.rs` | 1 match |
| 1 | `grep -n "pub struct ChannelCheckpointSignal" crates/roz-worker/src/checkpoint_writer.rs` | 1 match |
| 1 | `grep -n "pub mod checkpoint_signal" crates/roz-core/src/lib.rs` | 1 match |
| 1 | `cargo test -p roz-core --lib checkpoint_signal::tests::noop_signal_is_a_valid_checkpoint_signal` | PASS |
| 1 | `cargo test -p roz-worker --lib checkpoint_writer::tests::channel_signal_sends_tool_call_started_over_mpsc` | PASS |
| 2 | `grep -rn "checkpoint_signal" crates/roz-agent/src/agent_loop/` | 11 matches (≥ 4) |
| 2 | `grep -rn "tool_call_started\|tool_call_completed\|approval_received" crates/roz-agent/src/` | 5 matches (≥ 3) |
| 2 | `grep -n "with_checkpoint_signal" crates/roz-agent/src/agent_loop/mod.rs` | 2 matches (builder + test-wire) |
| 2 | `cargo test -p roz-agent --test agent_loop checkpoint_signal_tests::agent_loop_emits_tool_call_started_on_dispatch` | PASS |
| 2 | `cargo test -p roz-agent --test agent_loop checkpoint_signal_tests::agent_loop_emits_tool_call_completed_on_dispatch_return` | PASS |
| 2 | `cargo test -p roz-agent --test agent_loop checkpoint_signal_tests::agent_loop_emits_approval_received_on_resolve` | PASS |
| 2 | `cargo test -p roz-agent --test agent_loop checkpoint_signal_tests::agent_loop_with_noop_signal_is_backward_compatible` | PASS |
| 3 | `grep -n "emit_violation_event" crates/roz-worker/src/main.rs` | 4 matches (2 call sites + 2 doc refs) — was 0 |
| 3 | `grep -n "ChannelCheckpointSignal" crates/roz-worker/src/main.rs` | 3 matches (1 use + 2 doc refs) |
| 3 | `grep -n "with_checkpoint_signal" crates/roz-worker/src/main.rs` | 2 matches (1 call + 1 doc ref) |
| 3 | `cargo test -p roz-worker --lib dispatch::tests::emit_violation_event_after_reject_decision_lands_on_session_stream` | PASS |
| 3 | `cargo test -p roz-worker --lib dispatch::tests::emit_violation_event_after_halt_decision_lands_on_session_stream` | PASS |
| 3 | `cargo test -p roz-worker --lib dispatch::tests::emit_violation_event_after_clamp_decision_lands_on_session_stream` | PASS |
| Global | `cargo build --workspace` | PASS |
| Global | `cargo clippy -p roz-core -p roz-agent -p roz-worker --all-targets -- -D warnings` | PASS (clean) |
| Global | `cargo fmt --check -p roz-core -p roz-agent -p roz-worker` | PASS |
| Global | `cargo test -p roz-worker --lib` | 337 passed, 0 failed (was 329 pre-24-13; +8 new) |
| Global | `cargo test -p roz-core --lib` | 912 passed, 0 failed (was 910 pre-24-13; +2 new) |
| Global | `cargo test -p roz-agent --test agent_loop` | 69 passed, 0 failed (was 64 pre-24-13; +5 new) |
| Global | `cargo test -p roz-nats --lib` | 94 passed, 0 failed |
| Global | `cargo test -p roz-worker --test recovery_three_branches` | 7 passed |
| Global | `cargo test -p roz-worker --test phase24_e2e` | 2 passed, 1 ignored |

## Deviations from Plan

**None — plan executed exactly as written.**

The plan's `<interfaces>` block explicitly called out the "two intentional session-event emission paths" (broadcast direct for RecoveryPending; mpsc→broadcast forwarder for SafetyViolation) and the `ApprovalReceived` emission semantics. Both were honored verbatim; the Task 4 guard test (`agent_loop_does_not_emit_approval_received_on_denial`) pins the D-08 variant semantics explicitly.

One small in-scope-of-plan adjustment: the plan's `<action>` step 3 for Task 3 noted that `session_event_tx` is a `broadcast::Sender<EventEnvelope>` at main.rs:1739 — actual line is 1535 in the current file layout (the 24-12 hoist moved it). Functionality matches the plan text; only the line reference is stale.

## TDD Gate Compliance

Plan declares `tdd="true"` on all three tasks. Per the 24-12 precedent and the TDD execution protocol fail-fast rule, tests co-shipped with their implementations in a single commit per task:

- **Task 1** (`dc93cbf`): `CheckpointSignal` trait + `NoopCheckpointSignal` + `ChannelCheckpointSignal` + 7 tests in one commit. Splitting would require either a RED commit where `ChannelCheckpointSignal::new` doesn't exist (test doesn't compile, breaks bisect) or a stub that returns `todo!()` under `#[should_panic]` (misleading signal).
- **Task 2** (`62b5073`): AgentLoop field + builder + 3 emission sites + 5 tests in one commit. Same rationale — `with_checkpoint_signal` and the Phase-24 emission paths don't exist before the implementation commit; co-shipping is cleaner than mechanical RED-then-GREEN.
- **Task 3** (`86e7cf6`): execute_task wiring + 3 emission sites + 3 tests in one commit. The Reject/Halt/Clamp post-decision assertions are tested via `pre_dispatch_check` + `emit_violation_event` composition; both existed before this commit, so the tests alone would not have failed — the tests document the wired behavior.

## Known Stubs

None — every change is a production wire.

## Threat Flags

None new. The `ChannelCheckpointSignal` wraps an internal worker-local `mpsc::Sender` — no new network surface, no new trust boundary. The mpsc→broadcast forwarder routes the existing `SessionEvent::SafetyViolation` payload through the same broadcast sender the 24-12 `RecoveryPending` path publishes on; both paths go through the same downstream subscribers (session-scoped relay, health aggregator) with no change in trust semantics. The `emit_violation_event` call sites in `execute_task` run AFTER `write_safety_audit`, preserving the D-13 invariant that violations are persisted to `roz_safety_audit_log` independently of session-event delivery.

## User Setup Required

None — the wiring is invisible to operators. AgentLoop callers that do not opt into `with_checkpoint_signal` continue to use `NoopCheckpointSignal` with zero overhead (three empty method bodies). The per-task mpsc→broadcast forwarder is spawned only inside `execute_task`, so CLI, local runtime, and non-Phase-24 cloud sessions see no new tokio tasks.

## Next Phase Readiness

- **VERIFICATION.md gap 3 (FS-01 SC#1 — violations emit SafetyViolation)**: CLOSED. 2 production call sites in main.rs where previously there were 0.
- **VERIFICATION.md gap 8-remaining (FS-03 SC#1 — checkpoint writer event-driven triggers)**: CLOSED for the three agent-loop transitions. `CheckpointTrigger::DegradationChange` (4th variant) already has a production emitter from 24-12 Task 5. All four D-08 triggers are now grep-verifiable in production code.
- **Phase 27 SITL integration**: no new dependencies introduced here; the full outage→buffer→reconnect→replay→dedup scenario remains deferred per RD-01 and continues to rely on the deterministic `phase24_e2e.rs` scaffolding landed in 24-09.
- **Future plan — live resume physical-state plumbing**: unchanged — 24-12's deviation #1 noted that `handle_resume_instruction` always synthesizes `CrashState { brakes_engaged: false, joint_positions: None }` pending a future plan that threads `&CopperHandle` into the helper. 24-13 does not touch that path.

## Self-Check: PASSED

Files verified present in the worktree:
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-core/src/checkpoint_signal.rs` — FOUND (new file; `pub trait CheckpointSignal` at line 15).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-core/src/lib.rs` — FOUND (`pub mod checkpoint_signal` at line 8).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-worker/src/checkpoint_writer.rs` — FOUND (`pub struct ChannelCheckpointSignal` at line 92, 5 new tests in `mod tests`).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-agent/src/agent_loop/mod.rs` — FOUND (`checkpoint_signal: Arc<dyn CheckpointSignal>` field + `with_checkpoint_signal` builder).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-agent/src/agent_loop/dispatch.rs` — FOUND (trigger emission on pure + physical paths; `step_counter` parameter).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-agent/src/agent_loop/approvals.rs` — FOUND (`approval_received` emission on approved-resolve path only).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-agent/src/agent_loop/core.rs` — FOUND (`i64::from(cycles)` threaded to dispatch).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-agent/tests/agent_loop.rs` — FOUND (new `mod checkpoint_signal_tests` with 5 tests).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-worker/src/main.rs` — FOUND (session_event_tx parameter, mpsc→broadcast forwarder, 2 emit_violation_event call sites, ChannelCheckpointSignal wiring).
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-aa7557eb/crates/roz-worker/src/dispatch.rs` — FOUND (3 new post-decision emission tests).

Commits verified in `git log --oneline -4`:
- `dc93cbf feat(24-13): add CheckpointSignal trait + ChannelCheckpointSignal adapter` — FOUND.
- `62b5073 feat(24-13): AgentLoop emits checkpoint triggers at D-08 transitions` — FOUND.
- `86e7cf6 feat(24-13): wire ChannelCheckpointSignal + emit SafetyViolation events` — FOUND.

Build / lint / test summary:
- `cargo build --workspace`: PASS.
- `cargo clippy -p roz-core -p roz-agent -p roz-worker --all-targets -- -D warnings`: PASS (clean).
- `cargo fmt --check -p roz-core -p roz-agent -p roz-worker`: PASS.
- `cargo test -p roz-core --lib`: 912 passed (was 910 pre-24-13).
- `cargo test -p roz-worker --lib`: 337 passed (was 329 pre-24-13).
- `cargo test -p roz-agent --test agent_loop`: 69 passed (was 64 pre-24-13).
- `cargo test -p roz-nats --lib`: 94 passed.
- `cargo test -p roz-worker --test recovery_three_branches`: 7 passed.
- `cargo test -p roz-worker --test phase24_e2e`: 2 passed, 1 ignored.

---
*Phase: 24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery*
*Plan: 13*
*Completed: 2026-04-18*
