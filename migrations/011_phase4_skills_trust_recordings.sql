-- Phase 4: Skills enhancement, DeviceTrust, Recordings

-- Enhance roz_skills with new metadata columns
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'ai' CHECK (kind IN ('ai', 'execution'));
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS tags TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS platform TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS requires_confirmation BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS parameters JSONB NOT NULL DEFAULT '[]';
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS safety_overrides JSONB;
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS environment_constraints JSONB NOT NULL DEFAULT '[]';
ALTER TABLE roz_skills ADD COLUMN IF NOT EXISTS allowed_tools TEXT[] NOT NULL DEFAULT '{}';

-- Add content_hash to skill versions for dedup
ALTER TABLE roz_skill_versions ADD COLUMN IF NOT EXISTS content_hash TEXT;

-- DeviceTrust table
CREATE TABLE IF NOT EXISTS roz_device_trust (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES roz_hosts(id) ON DELETE CASCADE,
    posture TEXT NOT NULL DEFAULT 'untrusted' CHECK (posture IN ('trusted', 'provisional', 'untrusted')),
    firmware JSONB,
    sbom_hash TEXT,
    last_attestation TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, host_id)
);

ALTER TABLE roz_device_trust ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_device_trust
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Recordings table
CREATE TABLE IF NOT EXISTS roz_recordings (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    run_id UUID NOT NULL,
    environment_id UUID NOT NULL REFERENCES roz_environments(id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES roz_hosts(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK (source IN ('simulation', 'physical', 'hybrid')),
    channels JSONB NOT NULL DEFAULT '[]',
    duration_secs DOUBLE PRECISION NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE roz_recordings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_recordings
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Divergence reports table
CREATE TABLE IF NOT EXISTS roz_divergence_reports (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    sim_recording_id UUID NOT NULL REFERENCES roz_recordings(id) ON DELETE CASCADE,
    real_recording_id UUID NOT NULL REFERENCES roz_recordings(id) ON DELETE CASCADE,
    overall_score DOUBLE PRECISION NOT NULL,
    phases JSONB NOT NULL DEFAULT '[]',
    action TEXT NOT NULL CHECK (action IN ('pass', 'investigate', 'retune', 'escalate')),
    signatures_matched TEXT[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE roz_divergence_reports ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_divergence_reports
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_skills_kind ON roz_skills(kind);
CREATE INDEX IF NOT EXISTS idx_skills_tags ON roz_skills USING GIN(tags);
CREATE INDEX IF NOT EXISTS idx_device_trust_host ON roz_device_trust(host_id);
CREATE INDEX IF NOT EXISTS idx_recordings_run ON roz_recordings(run_id);
CREATE INDEX IF NOT EXISTS idx_recordings_env ON roz_recordings(environment_id);
