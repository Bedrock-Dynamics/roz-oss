---
phase: 24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery
plan: 15
subsystem: roz-server (integration tests)
tags: [gap-closure, integration-tests, live-http, nats-testcontainer, fs-01, verifier-closure]
gap_closure: true
dependency_graph:
  requires:
    - "crates/roz-server/src/routes/safety_policies.rs::create + fanout_policy_to_tenant (shipped in 24-11)"
    - "crates/roz-server/src/signing_gate.rs::SigningGate::sign_outbound + encrypt_signing_seed (shipped in Phase 23)"
    - "crates/roz-db/src/server_signing_state.rs::insert_server_signing_state (shipped in Phase 23)"
    - "crates/roz-worker/src/policy_enforcement.rs::apply_policy_push (shipped in 24-14 Task 2)"
    - "crates/roz-worker/src/signing_hooks.rs::WorkerSigningContext::verify_inbound_worker (shipped in Phase 23 Plan 23-08)"
    - "crates/roz-worker/src/dispatch.rs::pre_dispatch_check (shipped in 24-05)"
    - "crates/roz-test/src/pg.rs + nats.rs::pg_url + nats_container (pre-existing testcontainer fixtures)"
  provides:
    - "crates/roz-server/tests/phase24_policy_crud_live.rs — #[ignore]-gated live HTTP -> DB -> NATS -> worker-verify -> cache -> pre_dispatch_check round-trip"
    - "VERIFICATION.md human-action closure: \"live deployment for CRUD fan-out\" is now covered in-process by this test"
  affects:
    - "Phase 24 verification checklist — all but one residual item (Phase 27 SITL tick-level WASM clamp) is closed by automated tests"
tech-stack:
  added: []
  patterns:
    - "Full-router integration test pattern from `trust_gate_integration.rs`: `build_router` + `axum::serve` on ephemeral `TcpListener` + reqwest client with `bearer_auth`, seeded via `roz_test::pg_url()` / `roz_test::nats_container()`"
    - "Seed-encrypt + insert_server_signing_state pattern from `signing_gate.rs:555-585` for in-test signing-state bootstrap"
    - "Worker-side `WorkerSigningContext` with server-seeded verifying key — mirrored from `signing_hooks::tests::ctx` and `phase24_policy_pushstack.rs::build_worker_signing_ctx`"
key-files:
  created:
    - "crates/roz-server/tests/phase24_policy_crud_live.rs"
  modified: []
decisions:
  - "Use `StaticKeyProvider::from_key_bytes([7u8; 32])` (not `NullKeyProvider`) for BOTH the seed-encrypt helper AND `AppState.key_provider` — `SigningGate::sign_outbound` decrypts via the AppState provider, so the two sides must share key bytes or the round-trip fails inside `decrypt_signing_seed`."
  - "Request body's `policy_json` column must carry the full `PolicyV1` shape (not `null`) because `apply_policy_push` → `parse_policy_from_row` deserializes THAT column, not the flat `limits` column. `enforcement_mode=\"reject\"` is required so `pre_dispatch_check` returns `Reject(LimitExceeded)`; Clamp mode would return `Clamp` — same gotcha `phase24_policy_pushstack.rs:258-270` documents."
  - "NATS subject is `roz.policy.{host.name}`, not `roz.policy.{host.id}` — `fanout_policy_to_tenant` passes `h.name` to `Subjects::policy` (safety_policies.rs:282). Host name must also avoid NATS special chars (`.`, `*`, `>`), enforced by `Subjects::validate_token`."
  - "Worker context tenant_id / host_id match the DB signing state row's tenant_id / host_id. `WorkerSigningContext::verify_inbound_worker` does not cross-check those against the envelope (only direction + payload hash + server signature), but using the real IDs keeps the test shape parallel to production."
  - "Use `SignedDispatchEnforcement::Strict` — the weaker modes would accept unsigned or mis-signed envelopes and silently downgrade this to a cache-only test."
metrics:
  plan_start: "2026-04-18"
  plan_end: "2026-04-18"
  tasks_completed: 2
  commits: 2
  files_created: 1
  files_modified: 0
---

# Phase 24 Plan 15: Live-HTTP Policy CRUD Fan-Out Test Summary

## One-liner

Adds ONE `#[ignore]`-gated integration test that drives the FULL production HTTP → DB → NATS → worker-verify → cache → `pre_dispatch_check` path using Postgres + NATS testcontainers, closing the last "requires live deployment" verifier flag outside Phase 27 SITL scope.

## What the Test Proves

`crates/roz-server/tests/phase24_policy_crud_live.rs::http_policy_crud_fans_out_to_worker_and_gates_invocation` exercises the entire safety-policy fan-out chain end-to-end, in-process:

1. **Container bring-up** — `roz_test::pg_url()` + `roz_test::nats_container()` spin up ephemeral Postgres + NATS. Migrations applied via `roz_db::run_migrations`.
2. **DB seeding** — tenant, host (edge), API key, and `roz_server_signing_state` row (seed encrypted via `encrypt_signing_seed` against the same `StaticKeyProvider` wired into `AppState`).
3. **Live router spawn** — `build_router(state)` + `axum::serve` on an ephemeral `127.0.0.1:0` listener; REST client uses `reqwest::Client::post().bearer_auth()`.
4. **POST `/v1/safety-policies` with enforcement=reject** — non-trivial body carrying a full `PolicyV1` (`max_velocity.linear_m_per_s=1.0`, `angular_rad_per_s=0.5`, `enforcement_mode="reject"`). Response: `201 CREATED`, `data.id`, `data.version=1`, `data.name="live-e2e-test-policy"`.
5. **NATS wire observation** — a `nats.subscribe(Subjects::policy(&host.name))` started BEFORE the POST receives the fan-out envelope within 2 s. The `roz-sig-v1` header is pulled off the message.
6. **Worker-side signature verify** — `WorkerSigningContext::verify_inbound_worker` accepts the server's signature because its cached `server_verifying_key` was seeded with the same bytes the server persisted in `roz_server_signing_state.public_key_bytes`.
7. **Row parse + round-trip assertions** — `SafetyPolicyRow::deserialize(&msg.payload)` succeeds; `row.id` matches the HTTP `data.id`; `row.tenant_id` matches the tenant; `row.version == 1`; `limits.max_linear_m_per_s` is a number (round-trip preserved).
8. **`apply_policy_push`** — `PolicyCache::insert(row.id, ...)`, `HotPolicy::store(...)`, and `HotCopperPolicy::store(...)` all succeed (policy parses from `policy_json`).
9. **`pre_dispatch_check` rejects** — invocation with `safety_policy_id = row.id` + `declared_max_linear = Some(5.0)` produces `Reject(LimitExceeded { channel: "linear_velocity", value: 5.0, max: 1.0 })`, `decision.policy_id == embedded_policy_id` (from `PolicyV1.policy_id`), `decision.stale == false` (cache hit, not fallback).

The production paths exercised are not mocked: `fanout_policy_to_tenant` → `SigningGate::from_app_state` → `SigningGate::sign_outbound` (DB seed decrypt + `advance_sequence`) → `publish_signed` are all invoked by the handler exactly as they are in production.

## Anti-Tautology Check

Before the final clean run, the request body's `limits.max_velocity.linear_m_per_s` and `angular_rad_per_s` were both inverted to `100.0`. Expected failure: with 5.0 m/s linear now *under* the 100.0 m/s cap, `pre_dispatch_check` should return `Allow` instead of `Reject(LimitExceeded)`, failing the Step-9 assertion.

Observed failure (verbatim):
```
thread '...' panicked at .../phase24_policy_crud_live.rs:385:18:
expected Reject(LimitExceeded) on over-limit invocation, got Allow
```

Values restored to `1.0 / 0.5` before commit. The assertion is sensitive to the end-to-end flow of policy values from the HTTP body, through DB persist, through NATS fan-out, through `apply_policy_push`, to the `pre_dispatch_check` decision — not tautologically satisfied by any one stage alone.

## Deviations from Plan

### Auto-fixed during Task 1 (Rule 3 — blocking issues surfaced by plan text)

**1. [Rule 3 — Doc drift] Subject uses `host.name`, not `host.id.to_string()`**
- **Found during:** Task 1 Step 7 implementation
- **Issue:** Plan text said "subscribe to `Subjects::policy(&host_id.to_string())`", but `fanout_policy_to_tenant` at `routes/safety_policies.rs:282` constructs `worker_ids` as `(h.id, h.name)` and passes `h.name` (the String) to `Subjects::policy`. Subscribing on the UUID subject would never receive the fan-out.
- **Fix:** Subscribe on `Subjects::policy(&host_name)` where `host_name` is the alphanumeric+dash name used to create the host (must avoid NATS special chars `.`, `*`, `>`).
- **Files modified:** `crates/roz-server/tests/phase24_policy_crud_live.rs`

**2. [Rule 3 — Doc drift] Request body's `policy_json` must be the full `PolicyV1`, not `null`**
- **Found during:** Task 1 Step 8 implementation
- **Issue:** Plan text said `"policy_json": null`, but `apply_policy_push` → `parse_policy_from_row` deserializes `row.policy_json` (NOT the flat `limits` column) into a `PolicyV1`. `null` would produce `PolicyEnforcementError::PolicyParse` inside `apply_policy_push`, halting the test before the gate assertion. Additionally, the plan asserted `Reject(LimitExceeded { kind: "linear", .. })` but the actual enum variant has `channel` (not `kind`) and the string value is `"linear_velocity"` (not `"linear"`).
- **Fix:** Emit a complete `PolicyV1` JSON body (`policy_id`, `version`, `enforcement_mode="reject"`, full `limits` tree with `max_velocity`/`max_acceleration`/`max_force`, `deadman_timers`) and assert against the real field names.
- **Files modified:** `crates/roz-server/tests/phase24_policy_crud_live.rs`

**3. [Rule 3 — Doc drift] `StaticKeyProvider`, not `NullKeyProvider`**
- **Found during:** Task 1 Step 3 implementation
- **Issue:** Plan suggested `NullKeyProvider` as a fallback if `encrypt_signing_seed` was private. `encrypt_signing_seed` IS public (`signing_gate.rs:458`), and `NullKeyProvider` returns plaintext without round-trip-safe encryption — `SigningGate::sign_outbound` would then fail to decrypt the seed. Using `StaticKeyProvider::from_key_bytes([7u8; 32])` for both the seed-encrypt helper AND `AppState.key_provider` (same instance via `Arc::clone`) matches the production `signing_gate.rs` test helper `new_key_provider()`.
- **Fix:** Single `StaticKeyProvider` built at test startup, passed into both call sites.
- **Files modified:** `crates/roz-server/tests/phase24_policy_crud_live.rs`

These are documentation drift, not bugs in the shipping code — the production handler + worker logic is correct. Plan text was written against a mental model that needed grounding in current source.

### Residual `#[allow]` directives

- `#![allow(clippy::too_many_lines)]` — the test is one cohesive scenario; splitting would obscure the step-by-step narrative. Matches the precedent in `trust_gate_integration.rs:18`.
- `#![allow(clippy::float_cmp)]` — exact comparisons against hand-written policy values (`1.0`, `5.0`, `100.0`) are intentional; tolerance would mask an off-by-one breakage in the f64 pipeline.

## Residual Human-Verification Scope

**One** item remains outside fully-automated coverage after this plan:

### Copper 100 Hz tick-level clamping under a live OodaReAct WASM controller task

**Why this is deferred to Phase 27 SITL CI, not a gap in Phase 24:**

- The integration point is **already grep-verified** and exists in source: `crates/roz-copper/src/controller.rs:819-828` attaches `HotCopperPolicy` to each newly-loaded candidate's `HotPathSafetyFilter` via the `with_policy(hot_policy.clone())` builder when a `PreparedArtifact` is drained.
- The filter's clamping math is **unit-tested for latency**: `crates/roz-copper/src/safety_filter.rs::policy_clamp_under_5ms_budget` measures `policy_clamp` at ~8 ns/call (release mode) — four orders of magnitude under the 5 ms per-tick budget.
- The `HotCopperPolicy` → filter pointer round-trip is **integration-tested end-to-end** in `crates/roz-worker/tests/phase24_policy_pushstack.rs` (Plan 24-14 Task 3), which pushes a real signed policy row via NATS, calls `apply_policy_push`, and then calls `SafetyFilterTask::with_policy(copper_hot.clone()).policy_clamp(5.0, 2.0)` — asserting the clamp lands on the pushed 1.0/0.5 limits.
- What is NOT yet tested: the full `OodaReAct` agent loop firing `process(tick-input) → tick-output` at 100 Hz **in-process inside an integration test, through a compiled WASM controller artifact, with a live policy push mid-flight clamping its velocity outputs**.

The machinery required for that observation — `DeploymentManager`, compiled WASM artifact registration, `run_controller_loop_with_policy` with a real `PreparedArtifact` queue, Gazebo- or ArduPilot-sim-backed `io_grpc` — already has an integration-test home at `crates/roz-copper/tests/ardupilot_wasm_velocity.rs` and `crates/roz-copper/tests/drone_wasm_velocity.rs`, both of which require the Gazebo hardware sim and are run out-of-band, not in CI. Extending those tests with a policy-push mid-flight arm is the natural home for the last observation, and lives in **Phase 27 SITL CI scope** per CONTEXT.md.

This test (24-15) closes the gap that `fanout_policy_to_tenant` + `publish_policy_to_workers` + `WorkerSigningContext::verify_inbound_worker` + `apply_policy_push` all integrate correctly across a real signed-dispatch wire. The tick-level clamp under a running controller is one layer downstream — the plumbing is tested at every link; the remaining gap is observing the water flow through all of them at once under live WASM.

## Commits

| # | Hash | Message |
|---|------|---------|
| 1 | `c254bf7` | `test(24-15): prove HTTP POST /v1/safety-policies fans out via NATS to worker cache + gates invocation` |
| 2 | (this) | `docs(24-15): SUMMARY — close live-HTTP policy CRUD human-verification gap` |

## How to Run

```
cargo test -p roz-server --test phase24_policy_crud_live -- --ignored
```

Gates: Docker (Postgres 16-alpine + NATS with JetStream testcontainers). Expected runtime: 4–10 s (dominated by Postgres bring-up + migrations).

## Self-Check: PASSED

- `crates/roz-server/tests/phase24_policy_crud_live.rs` — FOUND
- commit `c254bf7` — FOUND (`git log --oneline | grep c254bf7`)
- `cargo test -p roz-server --test phase24_policy_crud_live -- --ignored` — PASS (4.15 s first run, 9.67 s clean run after anti-tautology cycle)
- `cargo fmt --check` — PASS
- `cargo clippy -p roz-server --all-targets -- -D warnings` — PASS
- STATE.md — untouched
- ROADMAP.md — untouched
