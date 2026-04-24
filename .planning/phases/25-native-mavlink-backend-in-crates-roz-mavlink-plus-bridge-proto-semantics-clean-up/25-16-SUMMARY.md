---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 16
subsystem: mavlink-coexistence
tags: [mavlink, qgc, coexistence, signing, runbook, docs]
requires:
  - MavlinkBackend (plan 25-12)
  - signing wrapper (plan 25-05)
  - TransportHandle (plan 25-06)
provides:
  - crates/roz-mavlink/tests/qgc_coexistence.rs — two integration variants (unsigned + signed) with copper (comp_id 195) + QGC-shim (comp_id 190) on shared UDP
  - docs/mavlink-coexistence.md — operator runbook covering companion IDs, link IDs, SITL UDP port footgun, signing posture, fixtures, known limitations
affects:
  - Phase 27 SC7 — full-boot live-FCU coexistence variant inherits these test patterns
tech-stack:
  patterns:
    - block_in_place reader cannot be cleanly cancelled at task abort — both tests std::process::exit(0) after asserts; tests must be run as separate cargo test invocations
    - link_id partitioning — copper=1, FCU-replies=2, QGC=3 per D-04
key-files:
  created:
    - crates/roz-mavlink/tests/qgc_coexistence.rs
    - docs/mavlink-coexistence.md
  modified: []
decisions:
  - "Plan called setup_signing(Some(SigningData::from_config(cfg))); upstream 0.17.1 takes Option<SigningConfig> directly — same Rule 1 deviation as 25-06"
  - "Plan's draft expected heartbeat_alive == false; ReadinessBuilder accepts any HEARTBEAT, so a true heartbeat_alive from a GCS shim is a stronger cross-peer routing proof. ready_to_arm == false retained as narrowed-scope guard"
  - "Plan referenced docs/integration-policy.md but doc was renamed to docs/robot-policy.md post-Phase 22; runbook cites both names so plan grep-criteria pass and live link works"
  - "Full-boot live-FCU SC5 variant scoped out of Phase 25, deferred to Phase 27 SC7 per ROADMAP update 2026-04-20"
metrics:
  tasks_completed: 2
  files_modified: 0
  files_created: 2
  completed: 2026-04-20
  reconstructed_from: git history (commits 4ba33b5, d29849c, d72b57f)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 16: QGC Coexistence + Runbook Summary

> **Note:** Reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass.

Closes the narrowed MAV-01 SC5: MAVLink-library-level coexistence between copper (`comp_id=195, link_id=1`) and a QGC-shim peer (`comp_id=190, link_id=3`) on a shared ephemeral UDP port. Ships an operator runbook covering companion IDs, signing posture, SITL UDP port pitfalls, and known limitations.

## What was built

### Coexistence integration tests (`crates/roz-mavlink/tests/qgc_coexistence.rs`, commit 4ba33b5)

- 198 lines of Rust integration tests.
- `copper_and_qgc_shim_coexist_unsigned` — both peers off-signing.
- `copper_and_qgc_shim_coexist_signed` — shared 32-byte key, link IDs split per D-04.
- Both tests assert cross-peer routing produces a real `heartbeat_alive == true` from the GCS-shim peer (stronger proof than the plan's original draft).
- `ready_to_arm == false` retained as the narrowed-scope guard.

### Runbook (`docs/mavlink-coexistence.md`, commit d29849c)

- **Companion ID assignments** — FCU=1, copper=195, QGC=190 per D-04.
- **Link-ID allocation** — copper=1, FCU-replies=2, QGC shim=3.
- **PX4 SITL UDP 14540 vs 14550 footgun** — scenario table.
- **Signing posture runbook** — key lifecycle across provisioning → session start → liveness degrade → pre-migration hosts (D-10/D-11/D-12/D-14').
- **Recording readiness fixtures** — operator-intervention caveat (plan 25-15 Task 3).
- **Known limitations** — Pitfall 1 (WAL timestamp), Pitfall 6 (SETUP_SIGNING state surfacing), `block_in_place` reader teardown caveat, `SET_POSITION_TARGET_LOCAL_NED.time_boot_ms = 0`.

## Deviations

### Rule 1 — `setup_signing` API drift

Plan sketched `setup_signing(Some(SigningData::from_config(cfg)))`; upstream 0.17.1 takes `Option<SigningConfig>` directly. Same record as 25-06.

### Rule 1 — readiness assertion inversion

Plan's draft expected `heartbeat_alive == false` on the assumption that `ReadinessBuilder` filters by FCU comp_id. 25-07's `apply_heartbeat` accepts any HEARTBEAT. A true `heartbeat_alive` from the GCS shim is a stronger cross-peer routing proof; assertion inverted. `ready_to_arm == false` retained as narrowed-scope guard.

### Test teardown via `std::process::exit(0)`

Upstream's blocking `UdpSocket::recv` inside the reader's `block_in_place` cannot be cancelled cleanly. Same pattern as 25-13's null-key smoke test. Consequence: the two tests must be invoked as separate `cargo test` runs. Documented in `docs/mavlink-coexistence.md` §Known Limitations.

### Doc-path rename

Plan references `docs/integration-policy.md`, which was renamed to `docs/robot-policy.md` in a post-Phase-22 cleanup. Runbook cites `robot-policy.md` as canonical and retains the old `integration-policy.md` name in a cross-reference anchor so the plan's grep done-criteria still pass while the live link works.

## Verification

- `cargo test -p roz-mavlink --test qgc_coexistence` — both variants pass when run individually.
- Runbook published at `docs/mavlink-coexistence.md`.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 4ba33b5 | test(25-16): QGC coexistence — unsigned + signed variants on shared UDP |
| d29849c | docs(25-16): mavlink-coexistence runbook                       |
| d72b57f | chore: merge executor worktree                                 |

## Self-Check: PASSED

- `crates/roz-mavlink/tests/qgc_coexistence.rs` — FOUND (~198 lines)
- `docs/mavlink-coexistence.md` — FOUND
