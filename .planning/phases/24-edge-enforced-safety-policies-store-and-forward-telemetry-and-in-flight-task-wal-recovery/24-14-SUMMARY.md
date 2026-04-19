---
phase: 24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery
plan: 14
subsystem: roz-worker (tests + refactors)
tags: [gap-closure, integration-tests, tautology-fix, nats-testcontainer, fs-01, fs-02, fs-03]
gap_closure: true
dependency_graph:
  requires:
    - "crates/roz-worker/src/main.rs::mpsc->broadcast SessionEvent forwarder (shipped inline in 24-13 Task 3)"
    - "crates/roz-worker/src/main.rs::policy push subscriber apply fan-out (shipped in 24-12 Task 5 + 24-13)"
    - "crates/roz-worker/src/telemetry.rs::publish_state_signed_with_buffer (shipped in 24-07 Task 1)"
    - "crates/roz-worker/src/telemetry_replay.rs::TelemetryReplay::run_once (shipped in 24-07 Task 2)"
    - "crates/roz-server/src/nats_handlers.rs::check_telemetry_dedup (shipped in 24-11 Task 2)"
    - "crates/roz-test/src/nats.rs::nats_container (pre-existing testcontainer fixture)"
  provides:
    - "roz_worker::session_event_forwarder::spawn_session_event_forwarder — public, testable helper"
    - "roz_worker::policy_enforcement::apply_policy_push — public async helper wrapping the cache/hot/copper_hot/DegradationChange apply fan-out"
    - "crates/roz-worker/tests/phase24_session_event_forwarder.rs — delivery contract test for the forwarder"
    - "crates/roz-worker/tests/phase24_policy_pushstack.rs — #[ignore]-gated NATS e2e for policy push -> cache -> copper filter clamp"
    - "crates/roz-worker/tests/phase24_outage_replay.rs — #[ignore]-gated NATS e2e for WAL -> replay -> dedup round-trip"
  affects:
    - "crates/roz-worker/src/lib.rs"
    - "crates/roz-worker/src/main.rs"
    - "crates/roz-worker/src/session_event_forwarder.rs (NEW)"
    - "crates/roz-worker/src/policy_enforcement.rs"
    - "crates/roz-worker/tests/phase24_session_event_forwarder.rs (NEW)"
    - "crates/roz-worker/tests/phase24_policy_pushstack.rs (NEW)"
    - "crates/roz-worker/tests/phase24_outage_replay.rs (NEW)"
tech-stack:
  added: []
  patterns:
    - "Extract-to-test pattern — inline tokio::spawn blocks inside execute_task were code-review-verified only. Pulling the forwarder + apply fan-out into standalone pub fn helpers in 24-14 Tasks 0 + 2 unblocks integration tests that can drive the exact production code paths without standing up the full execute_task apparatus."
    - "Forge-the-envelope pattern for worker-side receive tests — the server-side SigningGate hard-requires sqlx::PgPool (its tests use a fresh_pool + provision_server_signing_state helper ~80 LOC of Postgres boilerplate). The worker's signing contract is independent of the server's signing implementation — they just need to share a server-verifying-key seed. signing_hooks::tests::ctx() already seeds WorkerSigningContext with SERVER_SEED=[9u8;32]; re-using that seed to forge signed envelopes via roz_core::signing::sign_envelope gives the same coverage without the Postgres dependency. publish_policy_to_workers itself is unit-tested in crates/roz-server/src/routes/safety_policies.rs."
    - "WAL-as-ground-truth pattern for outage simulation — async_nats::Client with default options (retry_on_initial_connect=false, max_reconnects=None) does not produce a deterministic publish error on a closed socket: it queues and retries. The load-bearing observable effect of the outage branch in publish_state_signed_with_buffer is `wal.append_telemetry_frame` being called. Call that directly in tests to skip the flaky broken-client dance while still exercising the real replay + dedup pipeline."
    - "Anti-tautology sabotage cycle — every integration test was confirmed meaningful by temporarily inverting a load-bearing assertion (or swapping a policy value), running the test to watch it fail, then restoring the production code. Documented in each test's module docstring + in the commit message."
key-files:
  created:
    - crates/roz-worker/src/session_event_forwarder.rs
    - crates/roz-worker/tests/phase24_session_event_forwarder.rs
    - crates/roz-worker/tests/phase24_policy_pushstack.rs
    - crates/roz-worker/tests/phase24_outage_replay.rs
  modified:
    - crates/roz-worker/src/lib.rs
    - crates/roz-worker/src/main.rs
    - crates/roz-worker/src/policy_enforcement.rs
decisions:
  - "Forge server-to-worker envelopes directly via roz_core::signing::sign_envelope rather than spinning up a server-side SigningGate (which needs Postgres). Advisor-endorsed deviation — publish_policy_to_workers is already unit-tested; the genuine gap is the wire-to-worker-verify-to-cache-to-copper round-trip, which this approach exercises fully."
  - "Simulate NATS outage by calling wal.append_telemetry_frame directly instead of chasing a broken async_nats::Client. Advisor-endorsed deviation — the production outage branch's only side-effect is the same WAL append, and async_nats's default reconnect behaviour makes the broken-client shape non-deterministic."
  - "Keep TelemetryDedup + check_telemetry_dedup logic inline in the outage_replay test (10 LOC) rather than adding roz-server as an integration-test dev-dep. Adding roz-server would cascade into sqlx/axum/reqwest transitive compiles for two trivial generics. Source of truth stays at crates/roz-server/src/nats_handlers.rs:505-535 (already unit-tested); this test only verifies that real replayed envelopes produce novel sequence numbers at that interface."
  - "For Task 3 assertion 1, cover BOTH Clamp (the pushed policy's enforcement_mode) and Reject (a supplementary cache-insert). The plan text's combination of 'push Clamp policy' + 'assert Reject outcome' is internally inconsistent — enforce_command routes LimitExceeded through Clamp under Clamp-mode, not Reject. Preserving the push's Clamp semantics AND asserting the Reject vocabulary requires two cache entries, which is what the test does."
  - "Tasks 0 and 2 both ship refactor + original-behaviour-tests-unchanged: the existing 24-13 policy-push-apply path's unit tests and the forwarder-inside-execute_task behaviour are both preserved. No new RED-then-GREEN TDD cycle; the new integration tests are the coverage delta."
metrics:
  duration: "~1h 45min"
  completed: "2026-04-18"
  tasks_completed: 5
  commits: 6
  files_created: 4
  files_modified: 3
  integration_tests_added: 3
  anti_tautology_cycles: 3
---

# Phase 24 Plan 14: Three real e2e tests closing tautology gaps

**One-liner:** Closed the 24-VERIFICATION.md "11/11 automated checks passed" tautology gap by refactoring two inline blocks in `crates/roz-worker/src/main.rs` into testable helpers (Tasks 0 + 2) and adding three integration tests that drive the real wire / cache / replay code paths (Tasks 1 + 3 + 4) — each with a documented anti-tautology sabotage cycle.

## Outcome

Before this plan, `crates/roz-worker` held three Phase 24 delivery contracts that were covered only by static-grep or unit-tautological checks:

1. **mpsc → broadcast SessionEvent forwarder** (plan 24-13 Task 3). Inline tokio::spawn block inside `execute_task`. Tests asserted the individual `emit_violation_event` helper + the envelope struct shape, but nothing drove a `SessionEvent` through an mpsc channel and asserted it reappeared on a broadcast subscriber with fresh `EventId` + `CorrelationId` + `parent_event_id=None`.
2. **Policy push subscriber apply fan-out** (plans 24-12 + 24-13). Inline code inside the `roz.policy.{worker_id}` subscribe loop invoked `cache.insert`, `hot.store`, and `copper_hot.store` directly. No test drove a signed policy row through real NATS → worker verify → apply → `pre_dispatch_check` + `policy_clamp`.
3. **Telemetry outage → WAL → replay → server dedup** (plans 24-07 + 24-11). `publish_state_signed_with_buffer`'s success + WAL-fallback branches were unit-tested; `TelemetryReplay::run_once` was unit-tested; `check_telemetry_dedup` was unit-tested. Nothing drove the full loop end-to-end against a real NATS container.

After this plan:

- `roz_worker::session_event_forwarder::spawn_session_event_forwarder` is a public, testable helper replacing the inline 24-13 Task 3 block.
- `roz_worker::policy_enforcement::apply_policy_push` is a public, testable helper replacing the inline policy-push subscriber apply code.
- `crates/roz-worker/tests/phase24_session_event_forwarder.rs` drives three real `SessionEvent::SafetyViolation` values through the forwarder and asserts envelope shape + JoinHandle lifecycle.
- `crates/roz-worker/tests/phase24_policy_pushstack.rs` (`#[ignore]`-gated on Docker) publishes a real signed policy row onto a NATS testcontainer, receives it on the worker subscription side, runs `WorkerSigningContext::verify_inbound_worker`, parses + applies, and asserts the `pre_dispatch_check` / `SafetyFilterTask::policy_clamp` interfaces reflect the pushed limits.
- `crates/roz-worker/tests/phase24_outage_replay.rs` (`#[ignore]`-gated on Docker) runs a real NATS testcontainer through a 3-frame normal path, a 3-frame WAL-buffered "outage," a real `TelemetryReplay::run_once` drain, and a real-matching `check_telemetry_dedup` gate. All 6 frames land on the server subscription with strictly monotonic signed sequence numbers; duplicate re-feeds are rejected.

## Commits

| # | Commit | Task | Files |
|---|--------|------|-------|
| 1 | `954fafd` | Task 0 — Extract session-event forwarder into testable helper | `crates/roz-worker/src/session_event_forwarder.rs` (new), `crates/roz-worker/src/lib.rs`, `crates/roz-worker/src/main.rs` |
| 2 | `8bd6f24` | Task 1 — Forwarder delivery integration test | `crates/roz-worker/tests/phase24_session_event_forwarder.rs` (new) |
| 3 | `0b78bd7` | Task 2 — Extract `apply_policy_push` helper | `crates/roz-worker/src/policy_enforcement.rs`, `crates/roz-worker/src/main.rs` |
| 4 | `ab46e4d` | Task 3 — Policy push full-stack e2e (NATS testcontainer) | `crates/roz-worker/tests/phase24_policy_pushstack.rs` (new) |
| 5 | `99c4382` | Task 4 — Outage → WAL → replay → dedup e2e (NATS testcontainer) | `crates/roz-worker/tests/phase24_outage_replay.rs` (new) |
| 6 | (pending) | Task 5 — SUMMARY + rustfmt/clippy fixups | `.planning/phases/24.../24-14-SUMMARY.md` (new), rustfmt fixups on the above test files + `crates/roz-worker/src/main.rs`, clippy `while_let_loop` fix in the outage test |

## What Each Test Proves

### Test C — `phase24_session_event_forwarder.rs` (always-on, no Docker)

**Proves:** The mpsc → broadcast forwarder introduced in 24-13 Task 3 (now extracted in 24-14 Task 0) wraps every received `SessionEvent` in a fresh `EventEnvelope` with a unique `EventId`, a unique `CorrelationId`, `parent_event_id=None`, and an `Utc::now()` timestamp; that all three input events appear on the broadcast subscriber in order with fidelity to their `violation_kind` payloads; and that the forwarder's `JoinHandle` exits within 1 s of the mpsc sender being dropped.

**Anti-tautology:** Replaced the forwarder body with an immediate exit path (no mpsc drain, no broadcast send). The test failed at the first `mpsc_tx.send().await` because the receiver was dropped, confirming the test is genuinely driven by the forwarder's consumption of mpsc frames. Restored; test passes.

**Gap this closes:** 24-13 shipped the forwarder with its contract documented ("fresh EventId per envelope, parent=None, Utc timestamp") but with zero runtime verification. Code-review-only coverage is now integration-proven.

### Test A — `phase24_policy_pushstack.rs` (`#[ignore]`, requires Docker)

**Proves:** A signed `SafetyPolicyRow` published on `roz.policy.{worker_id}` over a real NATS testcontainer is (a) verified by `WorkerSigningContext::verify_inbound_worker` against the server seed, (b) parsed into a `SafetyPolicyRow`, (c) applied via `apply_policy_push` into `PolicyCache` + `HotPolicy` + `HotCopperPolicy`, and then (d) produces the correct `PreDispatchOutcome` + `policy_clamp` behaviour downstream. Covers Clamp, Reject, and Allow paths, plus the copper 100 Hz filter clamp reading through the same `HotCopperPolicy` pointer that the apply helper wrote to.

**Anti-tautology:** Swapped the pushed policy's `max_linear_m_per_s` and `max_angular_rad_per_s` to 100.0. Assertion 1 failed with "expected Clamp on pushed Clamp-mode policy, got Allow" because 5.0 m/s now falls under the 100.0 limit — the test is genuinely sensitive to the pushed values round-tripping onto the cache and the copper hot policy. Restored; test passes.

**Gap this closes:** Prior to 24-14, nothing drove a policy row through the wire end-to-end. The FS-01 acceptance path ("push → cache → pre_dispatch_check → copper safety filter") existed as grep-verified wiring. Integration-proven now.

### Test B — `phase24_outage_replay.rs` (`#[ignore]`, requires Docker)

**Proves:** Given a real NATS testcontainer and a worker surface populated with a `WalStore` + `WorkerSigningContext`, three normal-path frames via `publish_state_signed_with_buffer` land on NATS and leave the WAL empty; three outage-path frames appended directly to the WAL (simulating the production NATS-error fallback branch) are drained by `TelemetryReplay::run_once` back onto live NATS with fresh signing sequence numbers; all six frames land on the server subscription with strictly monotonic `SignedFields.sequence_number`; the real-matching `check_telemetry_dedup` accepts every novel seq and rejects re-feeds at the high-water mark and below.

**Anti-tautology:** Inverted the duplicate-rejection assertion (`assert!(!repeat_accepted, ...)` → `assert!(repeat_accepted, ...)`). Test failed at the high-water re-feed because `check_telemetry_dedup` correctly returns `false` for a replayed seq, so the inverted assertion panicked. Restored; test passes.

**Gap this closes:** FS-02's "telemetry outage → buffer → reconnect → replay → dedup drops duplicates" acceptance path previously had each component unit-tested but no integration test that drove all four stages together against real NATS. Now integration-proven.

## Deviations from Plan

### 1. Task 3 — Forge envelopes instead of using SigningGate + publish_policy_to_workers

**Rule 3 (blocking-issue deviation).** The plan text says to construct a server-side `SigningGate` that shares a key with `WorkerSigningContext` and call `publish_policy_to_workers`. `SigningGate::new` hard-requires a live `sqlx::PgPool` — pulling a Postgres testcontainer into this test adds ~80 LOC of `fresh_pool` + `provision_server_signing_state` boilerplate for zero additional coverage (since `publish_policy_to_workers` is already unit-tested in `crates/roz-server/src/routes/safety_policies.rs`).

**Resolution:** Forge the `roz-sig-v1` envelope directly using `roz_core::signing::sign_envelope` with the server signing key seed (`[9u8; 32]`) that `signing_hooks::tests::ctx()` already seeds `WorkerSigningContext` to trust. Matches the `signing_hooks::tests::sign_then_verify_round_trip_with_server_key` pattern exactly and exercises the gap that was actually uncovered (wire → worker-verify → cache → HotCopperPolicy → filter-clamp round-trip).

**Advisor-endorsed.**

### 2. Task 3 — Test BOTH Clamp and Reject, not just Reject

**Rule 3 (blocking-issue deviation).** The plan text says to push a Clamp-mode policy and then assert `PreDispatchOutcome::Reject`. These two statements are inconsistent — `enforce_command` routes `LimitExceeded` through `Clamp` under Clamp-mode (not Reject). Silently changing the pushed policy's `enforcement_mode` to `Reject` to satisfy the assertion would remove coverage of the Clamp vocabulary on the wire.

**Resolution:** The test asserts `PreDispatchOutcome::Clamp` on the pushed Clamp-mode policy (with `clamped_details` projecting onto the 1.0 m/s limit) AND inserts a second Reject-mode policy into the same `PolicyCache` to cover the Reject vocabulary. Zero regression to the plan's intent — all three enforcement modes + Allow are covered.

### 3. Task 4 — Direct WAL append instead of broken NATS client

**Rule 3 (blocking-issue deviation).** `async_nats::Client` with default options (`retry_on_initial_connect: false`, `max_reconnects: None` → infinite retries) does NOT produce a deterministic publish error on a closed socket; publishes queue and retry. Any broken-client shape (dead-port connect, drop-container-mid-test) is either non-deterministic or outright unreachable (dead-port connect errors at `.connect()` before returning a client).

**Resolution:** Simulate the outage by calling `wal.append_telemetry_frame(WORKER_ID, "state", &payload)` directly — the same call that `publish_state_signed_with_buffer`'s NATS-error → WAL branch makes at `crates/roz-worker/src/telemetry.rs:228`. The subsequent replay + dedup chain is exercised against live NATS exactly as in production.

**Advisor-endorsed.**

### 4. Task 4 — Inline minimal TelemetryDedup clone instead of roz-server dev-dep

**Rule 3 (blocking-issue deviation).** Adding `roz-server` as a `roz-worker` integration-test dev-dep would pull sqlx/axum/reqwest transitive compiles for two trivial generics (`type TelemetryDedup = Arc<Mutex<HashMap<String, u64>>>` + a ~10 LOC `check_telemetry_dedup` helper).

**Resolution:** Inline the same two definitions in the test file with a source-of-truth pointer to `crates/roz-server/src/nats_handlers.rs:505-535`. The server's unit tests pin the helper's semantics independently. This test proves that real replayed envelopes interface cleanly with that helper.

## How This Changes the Prior VERIFICATION.md Assessment

Before 24-14, the phase's verification score was `11/11 automated checks passed` (human_needed status). Those checks were:

- Grep-verified wiring (symbol present, call site exists)
- Unit tests with mocked inputs that tautologically produced the expected outputs
- Static structural tests (type signatures, field presence)

Three gap points were identified that the automated checks did not cover:

| Gap (pre-24-14) | Evidence | After 24-14 |
|-----------------|----------|-------------|
| FS-01 policy push → cache → HotCopperPolicy → filter clamp | Grep-verified wiring + unit tests over each leg | Integration-proven end-to-end against a real NATS testcontainer in `phase24_policy_pushstack.rs` |
| FS-02 telemetry outage → buffer → replay → dedup | Unit tests per leg (publish_state_signed_with_buffer, TelemetryReplay::run_once, check_telemetry_dedup) | Integration-proven end-to-end against a real NATS testcontainer in `phase24_outage_replay.rs` |
| FS-03 SessionEvent forwarder contract (fresh envelope IDs, parent=None, Utc timestamp) | Code-review only — no runtime driver | Integration-proven in `phase24_session_event_forwarder.rs` (always-on) with an explicit anti-tautology sabotage cycle |

## What Remains for Phase 27 SITL CI

Three scenarios remain deferred to Phase 27 SITL CI (RD-01) and are explicitly out of scope for 24-14:

1. **Full toxiproxy-induced outage.** The `phase24_induced_30s_nats_outage_survives_buffering_and_replay` test in `crates/roz-worker/tests/phase24_e2e.rs` already lays out the shape — bring up NATS behind a toxiproxy listener, start a real worker binary, inject a 30 s drop toxic, confirm the local watchdog stays un-latched, remove the toxic, assert clean replay + dedup. 24-14 covers the WAL-aware replay + dedup components; the outage-*detection* component remains SITL-bound.
2. **Live copper controller with a compiled WASM payload.** 24-14 exercises `SafetyFilterTask::policy_clamp` standalone with a `HotCopperPolicy` pointer. Running the full copper 100 Hz tick with a verified controller artifact under pushed-policy constraints remains Phase 27 scope.
3. **Hardware physical bring-up.** All 24-14 tests run against `:memory:` WAL + `testcontainers`-managed NATS. No physical robot or SITL simulator was exercised; the existing deferred acceptance path for hardware bring-up is unchanged.

## TDD Gate Compliance

Tasks 0 and 2 are refactor-only — the production behaviour is identical post-refactor, and the existing 24-13 tests (policy-enforcement unit tests, dispatch-side emission tests) continue to pass unchanged. No new RED-then-GREEN cycle was needed because the coverage delta this plan ships is the integration tests in Tasks 1 / 3 / 4, not new production behaviour.

Task 1 is pure test-only (`test(24-14): prove mpsc->broadcast forwarder...`) — the production helper it exercises shipped in Task 0.

Tasks 3 and 4 are also pure test-only — no new production code; the helpers they exercise already shipped (`apply_policy_push` in Task 2; `publish_state_signed_with_buffer` / `TelemetryReplay` / `check_telemetry_dedup` pre-24-14).

Anti-tautology cycles were performed for each integration test before commit and documented in the commit message + module docstring: break → run → observe failure → restore → re-run → observe pass.

## Known Stubs

None. Every change is production code.

## Threat Flags

None new. The extracted helpers expose internal worker state (mpsc/broadcast channels, policy cache pointers, copper hot-policy `ArcSwap`) that was already accessible to the inline blocks they replace. No new trust boundary, no new network surface. The integration tests open outbound connections only to the NATS testcontainer (`roz_test::nats_container`), which is the same fixture the rest of the workspace's integration tests use.

## User Setup Required

- Running the two `#[ignore]`-gated tests requires Docker + the `testcontainers` crate (already a workspace dev-dep). Run via:
  ```bash
  cargo test -p roz-worker --test phase24_policy_pushstack -- --ignored
  cargo test -p roz-worker --test phase24_outage_replay -- --ignored
  ```
- Task 1's `phase24_session_event_forwarder` test is always-on and runs under `cargo test -p roz-worker --test phase24_session_event_forwarder` with no external deps.

## Verification Commands

Run order used for final verification:

```bash
cargo fmt --check                                                   # PASS
cargo clippy --workspace --all-targets -- -D warnings               # PASS
cargo test -p roz-worker --test phase24_session_event_forwarder     # 1 passed
cargo test -p roz-worker --test phase24_policy_pushstack -- --ignored  # 1 passed (~0.5 s)
cargo test -p roz-worker --test phase24_outage_replay -- --ignored     # 1 passed (~2 s)
```

## Self-Check: PASSED

Files verified present in the worktree:
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/src/session_event_forwarder.rs` — FOUND
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/tests/phase24_session_event_forwarder.rs` — FOUND
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/tests/phase24_policy_pushstack.rs` — FOUND
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/tests/phase24_outage_replay.rs` — FOUND
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/src/lib.rs` — `pub mod session_event_forwarder;` registered
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/src/main.rs` — `spawn_session_event_forwarder` call replaces inline tokio::spawn block; `apply_policy_push` call replaces inline apply code in policy push subscriber
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/.claude/worktrees/agent-a5e2d17b/crates/roz-worker/src/policy_enforcement.rs` — `pub async fn apply_policy_push` present above `#[cfg(test)] mod tests`

Commits verified in `git log --oneline -6`:
- `954fafd refactor(24-14): extract session-event forwarder into testable helper` — FOUND
- `8bd6f24 test(24-14): prove mpsc->broadcast forwarder delivers SessionEvents with fresh envelope IDs` — FOUND
- `0b78bd7 refactor(24-14): extract apply_policy_push helper for e2e testability` — FOUND
- `ab46e4d test(24-14): prove policy push to cache to HotCopperPolicy to filter clamp round-trips via real NATS` — FOUND
- `99c4382 test(24-14): prove NATS outage -> WAL buffer -> reconnect replay -> dedup drops duplicate sequence numbers` — FOUND

Build / lint / test summary:
- `cargo fmt --check`: PASS (clean)
- `cargo clippy --workspace --all-targets -- -D warnings`: PASS (clean)
- `cargo test -p roz-worker --test phase24_session_event_forwarder`: 1 passed
- `cargo test -p roz-worker --test phase24_policy_pushstack -- --ignored`: 1 passed
- `cargo test -p roz-worker --test phase24_outage_replay -- --ignored`: 1 passed

---
*Phase: 24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery*
*Plan: 14*
*Completed: 2026-04-18*
