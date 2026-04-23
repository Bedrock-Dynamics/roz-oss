-- Migration: Phase 26.4 — Session metadata index (fleet query plane).
--
-- Per-session metadata + tool-call index populated at session finalize by
-- crates/roz-server/src/observability/metadata_index.rs::index_session. Both
-- tables are projections over roz_session_mcap_archives (the MCAP files remain
-- the forensic ground truth). This migration is the sole storage-layer
-- artifact for Phase 26.4.
--
-- Tables:
--   roz_session_metadata    — one row per session (PK: session_id).
--   roz_session_tool_calls  — one row per tool call (PK: session_id, call_id).
--
-- CHECK constraints:
--   * roz_session_metadata.outcome IN ('succeeded','failed','rejected','abandoned') — D-08
--   * roz_session_tool_calls.outcome IN ('succeeded','failed','unfinished')         — D-09
--
-- Nullability choices (D-10):
--   * latency_ms, finished_at nullable on roz_session_tool_calls to support
--     'unfinished' (aborted/crashed mid-tool) without a separate row flag.
--   * duration_ms, ended_at nullable on roz_session_metadata to support the
--     'abandoned' outcome (crash/idle/concurrent indexing — no terminal event).
--   * first_trace_id BYTEA nullable — empty or pre-26.3 archives yield NULL.
--
-- Indexes (D-18 + D-04):
--   * roz_session_metadata:
--     - PRIMARY KEY (session_id)
--     - (tenant_id, started_at DESC) — fleet time-range queries
--     - GIN on model_ids, policy_ids, controller_artifact_ids — @> containment
--   * roz_session_tool_calls:
--     - PRIMARY KEY (session_id, call_id)
--     - (tenant_id, requested_at DESC) — time-range queries
--     - (tenant_id, tool_name) — SC7 "tool_call_count > 50 AND tool_name = X"
--
-- rollover_index (D-04) — roz_session_tool_calls:
--   The rollover file (0-based index matching roz_session_mcap_archives.rollover_index)
--   that contains the tool call's ToolCallStarted (or Requested if Started absent).
--   Substrate drill-down uses (session_id, rollover_index, mcap_offset) to locate
--   the chunk containing the finalize message.
--
-- RLS:
--   Both tables ENABLE ROW LEVEL SECURITY with the project-wide
--   tenant_isolation policy. See crates/roz-db/src/lib.rs::set_tenant_context
--   for the authoritative setter.
--
-- Refs: .planning/phases/26.4-.../26.4-CONTEXT.md D-04/D-08/D-09/D-10/D-13/D-17/D-18/D-19/D-29;
--       migrations/20260420037_session_mcap_archives.sql (structural template).

BEGIN;

CREATE TABLE roz_session_metadata (
    session_id               UUID PRIMARY KEY,
    tenant_id                UUID NOT NULL REFERENCES roz_tenants(id),
    started_at               TIMESTAMPTZ NOT NULL,
    ended_at                 TIMESTAMPTZ,
    duration_ms              BIGINT,
    turn_count               INTEGER NOT NULL DEFAULT 0,
    tool_call_count          INTEGER NOT NULL DEFAULT 0,
    approval_count           INTEGER NOT NULL DEFAULT 0,
    intervention_count       INTEGER NOT NULL DEFAULT 0,
    violation_count          INTEGER NOT NULL DEFAULT 0,
    model_ids                TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    policy_ids               TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    controller_artifact_ids  TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    first_trace_id           BYTEA,
    outcome                  TEXT NOT NULL
                               CHECK (outcome IN ('succeeded','failed','rejected','abandoned')),
    error_summary            TEXT,
    indexed_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_session_metadata_tenant_started
  ON roz_session_metadata (tenant_id, started_at DESC);
CREATE INDEX idx_session_metadata_model_ids
  ON roz_session_metadata USING GIN (model_ids);
CREATE INDEX idx_session_metadata_policy_ids
  ON roz_session_metadata USING GIN (policy_ids);
CREATE INDEX idx_session_metadata_artifact_ids
  ON roz_session_metadata USING GIN (controller_artifact_ids);

ALTER TABLE roz_session_metadata ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_session_metadata
  USING (tenant_id = current_setting('rls.tenant_id')::uuid);

CREATE TABLE roz_session_tool_calls (
    session_id        UUID NOT NULL,
    call_id           TEXT NOT NULL,
    tenant_id         UUID NOT NULL REFERENCES roz_tenants(id),
    tool_name         TEXT NOT NULL,
    category          TEXT,
    requested_at      TIMESTAMPTZ NOT NULL,
    finished_at       TIMESTAMPTZ,
    latency_ms        BIGINT,
    had_approval      BOOLEAN NOT NULL DEFAULT FALSE,
    outcome           TEXT NOT NULL
                        CHECK (outcome IN ('succeeded','failed','unfinished')),
    trace_id          BYTEA,
    mcap_offset       BIGINT,
    rollover_index    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, call_id)
);

CREATE INDEX idx_session_tool_calls_tenant_requested
  ON roz_session_tool_calls (tenant_id, requested_at DESC);
CREATE INDEX idx_session_tool_calls_tool_name
  ON roz_session_tool_calls (tenant_id, tool_name);

ALTER TABLE roz_session_tool_calls ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_session_tool_calls
  USING (tenant_id = current_setting('rls.tenant_id')::uuid);

COMMIT;
