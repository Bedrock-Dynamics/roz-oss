---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 11
subsystem: hosts-key-provisioning
tags: [mavlink, signing, hosts, postgres, migration, key-provisioning]
requires:
  - mavlink_signing_key migration (plan 25-10 — stubbed locally per Rule 3)
  - signing_gate::encrypt_signing_seed from Phase 23 (key encryption reuse)
provides:
  - HostRow extended with mavlink_signing_key_ciphertext / _nonce / _version (Option-wrapped, #[serde(skip)] on secret material)
  - crate::roz_db::hosts::set_mavlink_signing_key — DB UPDATE over RETURNING *, takes any Executor
  - crate::roz_db::hosts::get_mavlink_signing_key — Option<(ciphertext, nonce, version)>; None for pre-migration / NULL rows
  - POST /v1/hosts auto-provisions a 32-byte MAVLink v2 signing seed in the same transaction as host insert
  - crates/roz-server/src/test_support.rs — shared AppState builders for REST/gRPC integration tests
affects:
  - plan 25-12 (backend-assembly) — backend reads signing key via get_mavlink_signing_key during boot
  - plan 25-13 (worker config + wiring) — worker provisioning reuses host endpoint
tech-stack:
  patterns:
    - Single-transaction invariant — encryption or persistence failure rolls back the whole host creation (D-23)
    - Plaintext seed zeroized post-use; ciphertext + nonce persisted, version safe to expose
    - Layering discipline — roz-db stays free of roz-server; encryption happens at route call-site
    - test_support.rs follows existing permissive_policy_for_integration_tests precedent (pub, not cfg(test))
key-files:
  created:
    - migrations/20260419036_mavlink_signing_key.sql (stub mirroring plan 25-10)
    - crates/roz-server/src/test_support.rs
  modified:
    - crates/roz-db/src/hosts.rs
    - crates/roz-server/src/lib.rs
    - crates/roz-server/src/routes/hosts.rs
decisions:
  - "Migration stub copied from plan 25-10 verbatim (Rule 3 deviation) so 25-11 integration test in this worktree can run against testcontainer Postgres; real file merges from 25-10"
  - "ciphertext + nonce serde-skipped so API responses never emit encrypted material (T-25-11-02); version is safe to expose"
  - "Migration uses all-or-none CHECK constraint, so partial NULL state is impossible — get_mavlink_signing_key returning None means pre-migration row"
metrics:
  tasks_completed: 2
  files_modified: 3
  files_created: 2
  completed: 2026-04-20
  reconstructed_from: git history (commits 42d2670, cb2c0cd, 3b27e54, df51105)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 11: Hosts Key Provisioning Summary

> **Note:** Reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass.

Auto-provision a MAVLink v2 signing key per host: extend `HostRow` with three Option-wrapped columns, expose `set_/get_mavlink_signing_key` helpers in `roz-db`, and have `POST /v1/hosts` generate + encrypt + persist the seed in the same transaction as the host insert.

## What was built

### `roz-db` extensions (commit cb2c0cd)

- `HostRow` gains `mavlink_signing_key_ciphertext: Option<Vec<u8>>`, `_nonce: Option<Vec<u8>>`, `_version: Option<i32>`.
- `ciphertext` + `nonce` carry `#[serde(skip)]` (T-25-11-02); `version` is safe to expose.
- `set_mavlink_signing_key` — pure DB `UPDATE ... RETURNING *`, takes any `Executor`.
- `get_mavlink_signing_key` — returns `Option<(ciphertext, nonce, version)>`; `None` on NULL.
- Pre-migration rows decode NULL columns to `None` via `sqlx::FromRow`.

### `roz-server` route + helpers (commits 3b27e54, df51105)

- `POST /v1/hosts` generates a 32-byte signing seed, encrypts via `signing_gate::encrypt_signing_seed` (Phase 23 reuse), persists ciphertext/nonce/version in the same transaction as the host row.
- Single-transaction invariant: any failure rolls back the whole host creation.
- Plaintext seed zeroized post-use.
- Adds `crates/roz-server/src/test_support.rs` with shared `AppState` builders (follows `permissive_policy_for_integration_tests` precedent: `pub`, not `cfg(test)`).

### Migration stub (commit 42d2670)

- `migrations/20260419036_mavlink_signing_key.sql` — 68-line stub mirroring plan 25-10's authoritative schema. Rule 3 deviation: 25-10 ran in parallel; stub allows the 25-11 integration test in this worktree to exercise the full INSERT → UPDATE sequence against the testcontainer Postgres.

## Verification

- `cargo build -p roz-db -p roz-server` clean.
- `cargo clippy --workspace -- -D warnings` clean (post-fix df51105).
- Integration test exercises full provisioning sequence against testcontainer Postgres.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 42d2670 | chore(25-11): add mavlink signing key migration stub           |
| cb2c0cd | feat(25-11): extend HostRow with MAVLink signing key columns   |
| 3b27e54 | feat(25-11): auto-provision MAVLink signing key on host creation |
| df51105 | fix(25-11): split long doc paragraphs to pass clippy           |

## Self-Check: PASSED

- `crates/roz-db/src/hosts.rs` — extended with signing-key columns
- `crates/roz-server/src/test_support.rs` — FOUND (~129 lines)
- `migrations/20260419036_mavlink_signing_key.sql` — FOUND
