-- Migration: Phase 26 — Session MCAP archives (OBS-01 D-01/D-06).
--
-- Per-session MCAP archive metadata. One row per session opened at
-- SessionStarted; updated to `finalized` at SessionCompleted or to a
-- recovery/idle-timeout terminal state. Rollover writes additional rows
-- with `rollover_index > 0` sharing a session_id.
--
-- Columns:
--   id              UUID   — row PK
--   tenant_id       UUID   — FK to roz_tenants(id); RLS scope key
--   session_id      UUID   — session identifier (not a FK — sessions live
--                           in agent_sessions but archives may exist
--                           for sessions not persisted to that table)
--   path            TEXT   — full filesystem path under ROZ_MCAP_DIR
--   size_bytes      BIGINT — file size after finalize; 0 while open
--   digest_sha256   BYTEA  — SHA-256 of final bytes; NULL while open
--   status          TEXT   — D-06: open|finalized|recovered_incomplete|finalized_idle_timeout
--   opened_at       TIMESTAMPTZ — SessionStarted wall-clock
--   finalized_at    TIMESTAMPTZ — SessionCompleted/timeout wall-clock; NULL while open
--   rollover_index  INT    — 0 for primary file; N for {session_id}.NNN.mcap (D-03)
--
-- CHECK constraints:
--   * status IN 4-value enum — D-06
--   * digest_iff_closed — digest+finalized_at NULL iff status='open'; NOT NULL otherwise
--
-- Indexes:
--   * tenant_id+session_id — export lookup (composite for ordering rollovers)
--   * status='open' partial — startup recovery scan (D-04)
--   * opened_at WHERE status<>'open' partial — retention FIFO sweep (D-02)
--
-- RLS:
--   * tenant_isolation uses current_setting('rls.tenant_id') matching
--     crates/roz-db/src/lib.rs::set_tenant_context (the authoritative name).
--     This matches existing roz_* tables like roz_mcp_servers
--     (see migrations/20260415033_roz_mcp_servers.sql line 90).
--
-- Refs: .planning/phases/26-.../26-CONTEXT.md D-01/D-02/D-03/D-04/D-06;
--       migrations/20260419036_mavlink_signing_key.sql (structural template).

BEGIN;

CREATE TABLE roz_session_mcap_archives (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES roz_tenants(id),
    session_id      UUID NOT NULL,
    path            TEXT NOT NULL,
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    digest_sha256   BYTEA,
    status          TEXT NOT NULL DEFAULT 'open'
                      CHECK (status IN ('open', 'finalized',
                                        'recovered_incomplete',
                                        'finalized_idle_timeout')),
    opened_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    finalized_at    TIMESTAMPTZ,
    rollover_index  INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT digest_iff_closed
      CHECK ((status = 'open' AND digest_sha256 IS NULL AND finalized_at IS NULL)
          OR (status <> 'open' AND digest_sha256 IS NOT NULL AND finalized_at IS NOT NULL))
);

CREATE INDEX idx_session_mcap_archives_tenant_session
  ON roz_session_mcap_archives (tenant_id, session_id, rollover_index);
CREATE INDEX idx_session_mcap_archives_open
  ON roz_session_mcap_archives (status) WHERE status = 'open';
CREATE INDEX idx_session_mcap_archives_retention
  ON roz_session_mcap_archives (opened_at) WHERE status <> 'open';

ALTER TABLE roz_session_mcap_archives ENABLE ROW LEVEL SECURITY;

-- RLS policy: use the same setting name as other roz_* tables
-- (rls.tenant_id per crates/roz-db/src/lib.rs::set_tenant_context).
CREATE POLICY tenant_isolation ON roz_session_mcap_archives
  USING (tenant_id = current_setting('rls.tenant_id')::uuid);

COMMIT;
