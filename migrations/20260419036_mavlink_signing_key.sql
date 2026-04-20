-- Migration: Phase 25 — MAVLink v2 signing key provisioning on roz_hosts (MAV-01 D-10).
--
-- Adds three additive columns to roz_hosts mirroring the roz_server_signing_state
-- encrypted-seed pattern from Phase 23 (see migrations/20260417035_device_keys.sql).
-- Backfill is operator-driven via host registration rotation — pre-existing rows
-- stay NULL and signing force-disables per D-12 with an operator-visible warning
-- logged at worker startup (see crates/roz-mavlink/src/signing.rs::build_signing_data).
--
-- Columns:
--   mavlink_signing_key_ciphertext  BYTEA  (32-byte AES-256-GCM ciphertext of a
--                                           URL-safe-no-pad base64-encoded
--                                           32-byte MAVLink v2 signing seed;
--                                           see roz_server::signing_gate::
--                                           encrypt_signing_seed)
--   mavlink_signing_key_nonce       BYTEA  (12-byte AES-GCM nonce)
--   mavlink_signing_key_version     SMALLINT  (1-based; bumped on rotation)
--
-- CHECK constraints:
--   * Nonce length = 12 (matches AES-GCM standard; same as roz_server_signing_state).
--   * key_version >= 1 (identity element; start at 1 on first provision).
--   * All-or-none: either all three columns set OR all three NULL. Partial state
--     would deterministically fail decryption and is a data-integrity bug.
--
-- Refs: .planning/phases/25-native-mavlink-backend-.../25-CONTEXT.md D-10..D-14;
--       migrations/20260417035_device_keys.sql (Phase 23 structural template).
--
-- Deviations from plan (Rule 2 — auto-add critical missing functionality):
--   * (none — straightforward additive schema)

BEGIN;

ALTER TABLE roz_hosts
    ADD COLUMN mavlink_signing_key_ciphertext  BYTEA,
    ADD COLUMN mavlink_signing_key_nonce       BYTEA,
    ADD COLUMN mavlink_signing_key_version     SMALLINT;

-- Integrity constraints match roz_server_signing_state (Phase 23 D-14).
-- Nonces are 12-byte AES-GCM; see crates/roz-core/src/key_provider.rs.
ALTER TABLE roz_hosts
    ADD CONSTRAINT roz_hosts_mavlink_signing_key_nonce_length
    CHECK (
        mavlink_signing_key_nonce IS NULL
        OR octet_length(mavlink_signing_key_nonce) = 12
    ),
    ADD CONSTRAINT roz_hosts_mavlink_signing_key_version_positive
    CHECK (
        mavlink_signing_key_version IS NULL
        OR mavlink_signing_key_version >= 1
    ),
    -- Either all three set or all three NULL; partial state would be a
    -- data-integrity bug (decrypt would fail deterministically).
    ADD CONSTRAINT roz_hosts_mavlink_signing_key_all_or_none
    CHECK (
        (mavlink_signing_key_ciphertext IS NULL
         AND mavlink_signing_key_nonce IS NULL
         AND mavlink_signing_key_version IS NULL)
        OR
        (mavlink_signing_key_ciphertext IS NOT NULL
         AND mavlink_signing_key_nonce IS NOT NULL
         AND mavlink_signing_key_version IS NOT NULL)
    );

-- No RLS policy changes: roz_hosts already has tenant-scoped RLS from its
-- original table creation. Additive columns inherit the existing policy.
-- (sqlx::migrate! runs as the DB superuser for schema; tenant-scoped reads
-- go through `SET LOCAL` at query time — unchanged by this migration.)

COMMIT;
