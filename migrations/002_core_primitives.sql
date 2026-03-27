-- Core primitives: environments, hosts, tasks, task_runs, triggers, skills, skill_versions, streams

CREATE TABLE IF NOT EXISTS roz_environments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('simulation', 'hardware', 'hybrid')),
    framework TEXT,
    config JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_hosts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    host_type TEXT NOT NULL CHECK (host_type IN ('cloud', 'edge', 'hybrid')),
    status TEXT NOT NULL DEFAULT 'offline' CHECK (status IN ('online', 'offline', 'degraded')),
    capabilities TEXT[] NOT NULL DEFAULT '{}',
    labels JSONB NOT NULL DEFAULT '{}',
    worker_version TEXT,
    clock_offset_ms DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    prompt TEXT NOT NULL,
    environment_id UUID NOT NULL REFERENCES roz_environments(id),
    skill_id UUID,
    host_id UUID REFERENCES roz_hosts(id),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'queued', 'provisioning', 'running', 'succeeded', 'failed', 'cancelled', 'safety_stop', 'retrying')),
    timeout_secs INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_task_runs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_id UUID NOT NULL REFERENCES roz_tasks(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id UUID REFERENCES roz_hosts(id),
    status TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled', 'safety_stop')),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    error_message TEXT
);

CREATE TABLE IF NOT EXISTS roz_triggers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    trigger_type TEXT NOT NULL CHECK (trigger_type IN ('schedule', 'webhook', 'mqtt', 'threshold', 'manual', 'integration')),
    config JSONB NOT NULL DEFAULT '{}',
    task_prompt TEXT NOT NULL,
    environment_id UUID NOT NULL REFERENCES roz_environments(id),
    enabled BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_skills (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_skill_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    skill_id UUID NOT NULL REFERENCES roz_skills(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (skill_id, version)
);

CREATE TABLE IF NOT EXISTS roz_streams (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    category TEXT NOT NULL CHECK (category IN ('telemetry', 'sensors', 'video', 'logs', 'events', 'commands')),
    host_id UUID REFERENCES roz_hosts(id),
    rate_hz DOUBLE PRECISION,
    config JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- RLS policies
ALTER TABLE roz_environments ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_hosts ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_tasks ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_task_runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_triggers ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_skills ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_skill_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_streams ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_environments
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_hosts
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_tasks
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_task_runs
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_triggers
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_skills
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_skill_versions
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_streams
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
