-- Migration: Phase 23 — two-direction Ed25519 signed dispatch + per-device key provisioning (FS-04).
--
-- Adds:
--   * roz_device_keys         — per-host Ed25519 public-key registry with versioning + rotation overlap + revocation.
--   * roz_server_signing_state — server-side outbound signing counter per (tenant_id, host_id, key_version).
--
-- Refs: .planning/research/DEEP-SIGN.md §4; .planning/phases/23-.../23-CONTEXT.md D-01..D-16;
--       .planning/phases/23-.../23-RESEARCH.md §File-Level Impact Map.
--
-- Deviations from plan (Rule 2 — auto-add critical missing functionality):
--   * Added `REFERENCES roz_tenants(id) ON DELETE CASCADE` on tenant_id (FK + cascade
--     match every other v2.0+ tenant-scoped table; referential integrity hole otherwise).
--   * Added `REFERENCES roz_hosts(id) ON DELETE CASCADE` on host_id (lifecycles track
--     host rows; orphaned key rows would never be reaped).
--   * Added RLS + tenant_isolation policy (universal on tenant-scoped tables:
--     20260415033_roz_mcp_servers.sql, 20260414028_agent_memory.sql, 20260416034_scheduled_tasks.sql).

BEGIN;

-- ---------------------------------------------------------------------------
-- roz_device_keys
--   One row per (tenant_id, host_id, key_version). On rotation a new row is
--   inserted with key_version + 1; the old row's `rotated_at` is set to now()
--   but the row remains active for a 24 h overlap window (D-07). Revocation
--   sets `revoked_at` and fails verification immediately (D-08).
-- ---------------------------------------------------------------------------
CREATE TABLE roz_device_keys (
    id                       UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id                  UUID        NOT NULL REFERENCES roz_hosts(id)   ON DELETE CASCADE,
    public_key_bytes         BYTEA       NOT NULL,                        -- 32-byte Ed25519 verifying key
    key_version              INT         NOT NULL,
    sequence_number_offset   BIGINT      NOT NULL DEFAULT 0,              -- high-water mark of last-verified seq (worker → server)
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    rotated_at               TIMESTAMPTZ,                                 -- set when a newer key_version is issued; old row still valid for 24 h
    revoked_at               TIMESTAMPTZ,                                 -- set by operator; fails verification immediately
    UNIQUE (tenant_id, host_id, key_version),
    CHECK (octet_length(public_key_bytes) = 32),
    CHECK (key_version >= 1),
    CHECK (sequence_number_offset >= 0)
);

-- Active-key lookup. D-16 correction: DEEP-SIGN.md §4 proposed
--   WHERE revoked_at IS NULL AND rotated_at IS NULL
-- which silently breaks the 24 h rotation overlap. Verifier selects rows by
-- explicit (host_id, key_version) from the envelope, so both overlap keys
-- must remain visible during the transition. Drop the rotated_at clause.
CREATE INDEX idx_device_keys_active ON roz_device_keys(host_id)
    WHERE revoked_at IS NULL;

CREATE INDEX idx_device_keys_tenant_host ON roz_device_keys(tenant_id, host_id);

ALTER TABLE roz_device_keys ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_device_keys
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);

-- ---------------------------------------------------------------------------
-- roz_server_signing_state (D-14)
--   Server-side outbound signing key material + monotonic counter. SEPARATE
--   from roz_device_keys because the server's signing key is per-server-
--   identity, not per-device, and mixing verify-state with sign-state on the
--   same row would race on rotation. One row per (tenant_id, host_id,
--   key_version); signing_key_bytes_encrypted is the AES-256-GCM ciphertext
--   of the server's 32-byte Ed25519 seed, encrypted with the existing
--   StaticKeyProvider (ROZ_ENCRYPTION_KEY). Public key is derivable and also
--   persisted for hot-path fetch.
-- ---------------------------------------------------------------------------
CREATE TABLE roz_server_signing_state (
    id                           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                    UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id                      UUID        NOT NULL REFERENCES roz_hosts(id)   ON DELETE CASCADE,
    key_version                  INT         NOT NULL,
    signing_key_bytes_encrypted  BYTEA       NOT NULL,                    -- AES-256-GCM ciphertext of 32-byte Ed25519 seed
    signing_key_nonce            BYTEA       NOT NULL,                    -- 12-byte nonce (see key_provider.rs)
    public_key_bytes             BYTEA       NOT NULL,                    -- 32-byte Ed25519 verifying key (derived from seed)
    sequence_number              BIGINT      NOT NULL DEFAULT 0,          -- monotonic, per row; advances on every outbound publish
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT now(),
    rotated_at                   TIMESTAMPTZ,
    UNIQUE (tenant_id, host_id, key_version),
    CHECK (octet_length(public_key_bytes) = 32),
    CHECK (octet_length(signing_key_nonce) = 12),
    CHECK (key_version >= 1),
    CHECK (sequence_number >= 0)
);

CREATE INDEX idx_server_signing_active ON roz_server_signing_state(tenant_id, host_id)
    WHERE rotated_at IS NULL;

ALTER TABLE roz_server_signing_state ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_server_signing_state
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);

COMMIT;
