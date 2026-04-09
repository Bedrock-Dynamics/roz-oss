---
phase: 7
reviewers: [codex]
reviewed_at: 2026-04-09
plans_reviewed: [07-01-PLAN.md, 07-02-PLAN.md]
---

# Cross-AI Plan Review — Phase 7

## Codex Review

### Plan 07-01: Foundation

**Summary**
The wave boundary is sensible, and the proposed NATS wiring matches existing server patterns, but the foundation has two correctness blockers: the current write path only keys off `model_digest`, and the request transaction commits after the handler returns. As written, this means calibration-only updates can be dropped entirely, and published change events can be observed before the DB commit is visible.

**Strengths**
- Reuses the existing optional NATS-in-`AppState` pattern cleanly.
- Keeps the event payload minimal: `host_id` and `tenant_id` are enough if handlers always re-read from Postgres.
- Adds a real publish-path integration test instead of only unit tests.
- Uses an internal subject namespace, which is appropriate for server-only fanout.

**Concerns**
- **HIGH**: Calibration-only changes are impossible to support with the current persistence gate. `conditional_upsert` only updates when `embodiment_model->>'model_digest'` changes in `crates/roz-db/src/embodiments.rs:73`. Since `update_embodiment` relies on that result in `crates/roz-server/src/routes/hosts.rs:189`, a new runtime/calibration with the same model digest is neither persisted nor published.
- **HIGH**: Publishing from `update_embodiment` before commit creates a lost-update race. The handler runs inside `Tx`, but auto-commit happens after the handler returns in `crates/roz-server/src/middleware/tx.rs:159`. A watcher can receive the NATS event, re-read old state, skip the delta, and never get another event.
- **HIGH**: Fire-and-forget publish failure is not acceptable if this PUT is the only notification source. A successful PUT with a dropped publish leaves active streams stale until some later reconnect or write. With D-02 rejecting polling fallback, this is a correctness gap, not just degraded availability.
- **MEDIUM**: Wave 1 likely won't compile if it adds new proto RPCs without also adding temporary trait impl stubs. The current `EmbodimentService` impl in `crates/roz-server/src/grpc/embodiment.rs:146` only covers the existing RPC set.
- **LOW**: The publish test proves message arrival, but not commit visibility, and `sleep(50ms)` is weaker than an explicit flush/synchronization point.

**Suggestions**
- Fix persistence first: gate writes on both model and runtime change, ideally using a runtime digest such as `combined_digest` or `calibration_digest`, not only `model_digest`.
- Do not publish before commit. Best option is an outbox/post-commit publisher; minimum option is explicit `tx.commit().await` before publish, with clear acceptance of at-least-once semantics.
- If wave separation is required, add `UNIMPLEMENTED` stream stubs in wave 1 so the proto change does not break the build.
- Add a test that sends same-model/new-runtime data and asserts both DB state and notification behavior.

**Risk Assessment**
**HIGH** — this wave contains the change-detection and ordering guarantees the stream layer depends on, and both are currently unsound.

---

### Plan 07-02: Handlers

**Summary**
The handler design is directionally good: subscribe-before-read is the right race-avoidance pattern, the frame-tree digest is correctly scoped to frame-tree state instead of full-model state, and snapshot/delta/keepalive is a reasonable wire contract. The main gaps are calibration delta completeness, terminal error semantics, and missing end-to-end verification of actual streaming behavior.

**Strengths**
- Correctly rejects full-model digest reuse for frame-tree streaming; that avoids false positives when unrelated model fields change.
- Subscribe-before-read is the right pattern for initial snapshot + follow-on deltas.
- The symmetric `oneof { snapshot | delta | keepalive }` contract is easy for clients to consume.
- Tenant ownership is checked both on connect and again on event handling, which is good defense in depth.
- `mpsc` + `ReceiverStream` matches existing server streaming style.

**Concerns**
- **HIGH**: `CalibrationDelta` is incomplete relative to the actual domain type in `crates/roz-core/src/embodiment/calibration.rs:23`. The plan omits `stale_after`, `temperature_range`, `valid_for_model_digest`, and an explicit way to represent overlay removal after the stream is already open. As written, a client cannot reliably reconstruct calibration state from snapshot + delta.
- **HIGH**: The plan has helper unit tests, but no E2E stream tests for the actual success criteria: initial snapshot, real delta after PUT, keepalive behavior, and terminal failure on NATS loss. That leaves STRM-01/02/03/04 under-verified.
- **MEDIUM**: `compute_calibration_digest` should reuse the canonical digest contract already defined in `crates/roz-core/src/embodiment/calibration.rs:51` or the stored runtime digest, not invent a second hashing path.
- **MEDIUM**: D-10 says NATS drop should end the stream with `Status::internal`, but the closest existing pattern in `crates/roz-server/src/grpc/tasks.rs:513` exits quietly on stream end. If this plan copies that behavior, it will violate the stated failure contract.
- **LOW**: A hardcoded 15s heartbeat may be fine, but it should be configurable if infra idle timeouts vary.

**Suggestions**
- Simplify calibration delta. A whole-overlay replacement delta is probably better than a field-by-field patch here; it still satisfies "snapshot then delta" while avoiding patch incompleteness.
- If fine-grained calibration delta is kept, add every mutable field plus explicit "overlay cleared" semantics.
- Use canonical digest sources: frame-tree digest may be server-generated and opaque, but calibration should come from `CalibrationOverlay::compute_digest()` or the runtime's `calibration_digest`.
- Add E2E tests for `StreamFrameTree` and `WatchCalibration`: connect, assert snapshot, perform PUT, assert delta, assert keepalive, assert `FAILED_PRECONDITION` with no NATS, and assert terminal `INTERNAL` on NATS drop.
- Make the handler explicitly send an error item on NATS closure, not just end the stream.

**Risk Assessment**
**HIGH** — the core shape is good, but the calibration delta contract is not complete enough yet, and the lack of end-to-end stream tests leaves the phase goals insufficiently proven.

---

## Consensus Summary

Single reviewer (codex). No cross-AI consensus possible — findings are single-source but highly specific and reference concrete file paths and line numbers in the codebase.

### Key Concerns (all HIGH severity)

1. **Persistence gate is model-digest-only (Plan 01)** — `conditional_upsert` silently drops calibration-only updates. This is a prerequisite bug that breaks WatchCalibration before any streaming code runs.

2. **Publish-before-commit race (Plan 01)** — NATS event fires while `Tx` is still open. Subscribers can read stale DB state. Must commit before publishing, or use a transactional outbox.

3. **Fire-and-forget publish loses events (Plan 01)** — D-02 forbids polling fallback, so a dropped publish permanently desyncs active streams until the next PUT. Needs at-least-once semantics or publish retry.

4. **CalibrationDelta incomplete (Plan 02)** — Missing `stale_after`, `temperature_range`, `valid_for_model_digest`, and overlay-clear semantics. Client cannot reconstruct full state from snapshot + deltas. Consider whole-overlay replacement instead of field patching.

5. **No E2E stream tests (Plan 02)** — Only helper unit tests. STRM-01/02/03/04 success criteria not actually verified by any automated test. Need tests for: initial snapshot, delta-after-PUT, keepalive, FAILED_PRECONDITION, terminal INTERNAL on NATS drop.

### Medium Concerns

- Wave 1 compilation risk: proto RPCs added without trait impl stubs may break `cargo build`.
- Duplicate digest path: reinventing calibration digest instead of reusing `CalibrationOverlay::compute_digest()`.
- D-10 compliance: `StreamTaskStatus` pattern exits quietly on NATS close — needs explicit `Status::internal` send to honor D-10.

### Plan-Level Strengths (agreed)

- NATS-on-PUT wiring matches existing server patterns.
- Minimal event payload (host_id + tenant_id) with DB re-read is correct.
- Frame-tree-only digest (not full-model digest) was correctly caught in revision.
- Subscribe-before-read race avoidance.
- Symmetric oneof snapshot/delta/keepalive wire contract.
- mpsc + ReceiverStream follows established patterns.

### Divergent Views

N/A (single reviewer).
