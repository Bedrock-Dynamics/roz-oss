-- Migration: Phase 26.7 — Session artifacts (D-01/D-06).
--
-- Generic per-session sidecar-artifact metadata. One row per physical
-- file uploaded to ROZ_ARTIFACT_DIR. 'mcap' in the CHECK enum is
-- reserved for forward compat (D-03) — MCAPs continue to live in
-- roz_session_mcap_archives this phase; no 'mcap' rows are written here.
--
-- Columns:
--   artifact_id    UUID  — row PK
--   tenant_id      UUID  — FK to roz_tenants(id); RLS scope key
--   session_id     UUID  — session identifier (not a FK — matches
--                          roz_session_mcap_archives precedent)
--   artifact_type  TEXT  — 'mcap'|'copper'|'ulog'|'video'|'bundle'
--   path           TEXT  — storage path relative to ROZ_ARTIFACT_DIR
--   digest_sha256  BYTEA — SHA-256 of bytes; 32 bytes
--   size_bytes     BIGINT
--   content_type   TEXT  — informational MIME
--   uploaded_at    TIMESTAMPTZ
--
-- Indexes:
--   * (tenant_id, session_id, artifact_type) — bundle/session lookup (D-05)
--   * (uploaded_at)                          — retention TTL pass (D-05)
--
-- RLS:
--   * tenant_isolation uses current_setting('rls.tenant_id')::uuid
--     matching crates/roz-db/src/lib.rs::set_tenant_context.
--
-- Note: session_id is NOT a foreign key. A caller with valid auth can
-- upload under any session_id they choose; RLS + tenant_id column is the
-- authority. Cross-tenant fabrication is blocked by RLS, not by the FK
-- surface — same contract as roz_session_mcap_archives.
--
-- Refs: .planning/phases/26.7-.../26.7-CONTEXT.md D-01..D-05;
--       migrations/20260420037_session_mcap_archives.sql (structural template).

BEGIN;

CREATE TABLE roz_session_artifacts (
    artifact_id    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      UUID NOT NULL REFERENCES roz_tenants(id),
    session_id     UUID NOT NULL,
    artifact_type  TEXT NOT NULL
                     CHECK (artifact_type IN ('mcap','copper','ulog','video','bundle')),
    path           TEXT NOT NULL,
    digest_sha256  BYTEA NOT NULL,
    size_bytes     BIGINT NOT NULL,
    content_type   TEXT NOT NULL,
    uploaded_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(session_id, artifact_type, path)
);

CREATE INDEX idx_session_artifacts_tenant_session
  ON roz_session_artifacts (tenant_id, session_id, artifact_type);
CREATE INDEX idx_session_artifacts_retention
  ON roz_session_artifacts (uploaded_at);

ALTER TABLE roz_session_artifacts ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_session_artifacts
  USING (tenant_id = current_setting('rls.tenant_id')::uuid);

COMMIT;
