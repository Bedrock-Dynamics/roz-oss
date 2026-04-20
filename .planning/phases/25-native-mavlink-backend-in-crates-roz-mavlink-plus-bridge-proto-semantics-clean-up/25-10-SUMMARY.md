---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 10
subsystem: persistence
tags: [mavlink, migration, postgres, schema, signing-key]
requires:
  - migrations/20260417035_device_keys.sql (Phase 23 encrypted-seed template)
  - roz_hosts (pre-existing tenant-scoped table with RLS)
provides:
  - roz_hosts.mavlink_signing_key_ciphertext column
  - roz_hosts.mavlink_signing_key_nonce column
  - roz_hosts.mavlink_signing_key_version column
  - roz_hosts_mavlink_signing_key_nonce_length CHECK
  - roz_hosts_mavlink_signing_key_version_positive CHECK
  - roz_hosts_mavlink_signing_key_all_or_none CHECK
affects:
  - crates/roz-db (sqlx::migrate! picks up the new file at compile time)
  - future plan 25-11 (hosts::create extends to populate the new columns)
  - future plan 25-05 (build_signing_data reads seed from these columns)
tech-stack:
  added: []
  patterns:
    - "AES-256-GCM encrypted-seed triplet (ciphertext, nonce, version) mirroring Phase 23 roz_server_signing_state"
    - "Additive schema with all-or-none CHECK constraint to force atomic provisioning"
    - "Operator-driven backfill via host registration rotation (no UPDATE statement)"
key-files:
  created:
    - migrations/20260419036_mavlink_signing_key.sql
  modified: []
decisions:
  - "Columns remain NULLABLE so pre-existing hosts are detectable at worker boot (D-12 fail-safe: signing force-disabled + warning logged)"
  - "No backfill UPDATE — pre-migration hosts rotate through host-registration path, not SQL"
  - "No separate down migration — sqlx::migrate! is forward-only per project convention"
  - "Shape matches roz_server_signing_state exactly so roz_server::signing_gate::encrypt_signing_seed works unchanged"
metrics:
  duration: "~4 minutes (includes 2m11s cargo build)"
  completed_date: "2026-04-20"
  tasks: 1
  files_created: 1
  files_modified: 0
---

# Phase 25 Plan 10: MAVLink Signing Key Migration Summary

One-liner: Added Phase 25 migration `20260419036_mavlink_signing_key.sql` adding three additive encrypted-seed columns (`mavlink_signing_key_ciphertext`, `_nonce`, `_version`) to `roz_hosts` with nonce-length, version-positivity, and all-or-none CHECK constraints, mirroring the Phase 23 `roz_server_signing_state` shape so existing AES-256-GCM encryption helpers work unchanged.

## What Was Built

- `migrations/20260419036_mavlink_signing_key.sql` — 68-line atomic migration that:
  - Adds three additive nullable columns to `roz_hosts` (`mavlink_signing_key_ciphertext BYTEA`, `mavlink_signing_key_nonce BYTEA`, `mavlink_signing_key_version SMALLINT`)
  - Adds three CHECK constraints: nonce length = 12 bytes (AES-GCM standard), `key_version >= 1`, and all-three-set-or-all-three-NULL (prevents partial-state data-integrity bugs)
  - Is BEGIN/COMMIT-wrapped so any constraint violation triggers atomic rollback
  - Preserves existing RLS posture on `roz_hosts` (additive columns inherit tenant-scoped policy — no policy changes required)
  - Carries a header comment citing Phase 25 D-10..D-14 and the Phase 23 structural template

## Tasks Completed

| Task | Name                                                | Commit  | Files                                                    |
| ---- | --------------------------------------------------- | ------- | -------------------------------------------------------- |
| 1    | Create migrations/20260419036_mavlink_signing_key.sql | 6709633 | migrations/20260419036_mavlink_signing_key.sql (created) |

## Verification

- `test -f migrations/20260419036_mavlink_signing_key.sql`: PASS
- `grep -q 'mavlink_signing_key_ciphertext  BYTEA'`: PASS (spacing matches column-alignment convention)
- `grep -q 'mavlink_signing_key_nonce       BYTEA'`: PASS
- `grep -q 'mavlink_signing_key_version     SMALLINT'`: PASS
- `grep -q 'roz_hosts_mavlink_signing_key_nonce_length'`: PASS
- `grep -q 'roz_hosts_mavlink_signing_key_version_positive'`: PASS
- `grep -q 'roz_hosts_mavlink_signing_key_all_or_none'`: PASS
- `grep -q 'octet_length(mavlink_signing_key_nonce) = 12'`: PASS
- `grep -q 'BEGIN;'` and `grep -q 'COMMIT;'`: PASS
- File ends with `0a` (POSIX newline): PASS (verified via `tail -c 1 | xxd`)
- `cargo build -p roz-db`: PASS (2m11s, clean build; sqlx::migrate! picks up the new file at compile time)

## Deviations from Plan

None — plan executed exactly as written. The action block's embedded SQL was copied verbatim.

## Threat Model Coverage

All four entries from the plan's `<threat_model>` map directly to artifacts in the migration:

| Threat ID | Disposition | Implementation |
| --------- | ----------- | -------------- |
| T-25-10-01 (Info-Disclosure: plaintext seed) | mitigate | Column types are `BYTEA` for ciphertext + 12-byte nonce CHECK forces AES-GCM shape |
| T-25-10-02 (Tampering: partial insert)       | mitigate | `roz_hosts_mavlink_signing_key_all_or_none` CHECK rejects partial triplets |
| T-25-10-03 (Info-Disclosure: NULL fail-open) | mitigate | Columns kept nullable to detect pre-existing hosts; worker-side `build_signing_data` returns `None` + logs warning (handled in future plan 25-05) |
| T-25-10-04 (DoS: non-atomic migration)       | mitigate | BEGIN/COMMIT wraps all ALTER TABLE statements |

## Threat Flags

None — this migration introduces no security-relevant surface beyond what the plan's threat model already covers.

## Known Stubs

None.

## Key Decisions

1. **Columns remain nullable.** D-12 handles pre-existing hosts operationally — `NULL` state at worker startup forces signing off and logs a warning rather than failing boot.
2. **No backfill UPDATE.** D-12 defers backfill to host-registration rotation; a SQL data migration would require generating/encrypting seeds without the encryption-key provider, which only runs in-process.
3. **Exact column-name parity with `roz_server_signing_state`.** Keeps the existing `encrypt_signing_seed` helper reusable verbatim in plan 25-11.
4. **All-or-none CHECK.** Partial state would deterministically fail decryption at runtime; rejecting it at write-time surfaces misconfiguration immediately instead of at worker boot.

## Next Steps

- Plan 25-11 extends `crates/roz-db/src/hosts.rs::create` to populate these columns on first host provision.
- Plan 25-05 wires `crates/roz-mavlink/src/signing.rs::build_signing_data` to read the decrypted seed (or return `None` + warn per D-12).

## Self-Check: PASSED

- Created file exists:
  - `migrations/20260419036_mavlink_signing_key.sql`: FOUND
- Commits exist:
  - `6709633`: FOUND (`feat(25-10): add migration for mavlink signing key on roz_hosts`)
