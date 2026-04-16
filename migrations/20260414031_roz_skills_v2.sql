-- Phase 18 SKILL-01: per-tenant skill library with composite PK (tenant, name, version).
--
-- BROWNFIELD REPLACEMENT: Phase 4 shipped roz_skills + roz_skill_versions with a
-- surrogate-key PK model that never acquired a production write path (the legacy
-- /v1/skills REST route is absent — verified by grep). Phase 18 adopts
-- agentskills.io-compatible progressive disclosure: inline body_md + JSONB
-- frontmatter mirror + external object-store for bundled assets (CONTEXT D-03).
-- The Phase 4 schema is not exposed by any production write path; no production
-- data loss on drop. CASCADE removes Phase-4 indexes and triggers from migration 011.

DROP TABLE IF EXISTS roz_skill_versions CASCADE;
DROP TABLE IF EXISTS roz_skills CASCADE;

CREATE TABLE roz_skills (
    tenant_id    UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name         TEXT        NOT NULL,
    version      TEXT        NOT NULL,
    body_md      TEXT        NOT NULL,
    frontmatter  JSONB       NOT NULL,
    source       TEXT        NOT NULL DEFAULT 'local',
    created_by   TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name, version),
    CHECK (length(frontmatter->>'description') > 0 AND length(frontmatter->>'description') <= 1024),
    CHECK (length(name) > 0 AND length(name) <= 64),
    CHECK (name ~ '^[a-z0-9]+(-[a-z0-9]+)*$')
);

CREATE INDEX roz_skills_tenant_recent ON roz_skills (tenant_id, created_at DESC);
CREATE INDEX roz_skills_name ON roz_skills (tenant_id, name);
CREATE INDEX roz_skills_frontmatter_gin ON roz_skills USING GIN (frontmatter);

CREATE OR REPLACE FUNCTION roz_skills_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_skills_touch_trg
    BEFORE UPDATE ON roz_skills
    FOR EACH ROW EXECUTE FUNCTION roz_skills_touch_updated_at();

ALTER TABLE roz_skills ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_skills
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
