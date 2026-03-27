-- Control primitives: commands, capability_leases, safety_policies, run_provenance, frame_contracts, degradation_modes

CREATE TABLE IF NOT EXISTS roz_commands (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES roz_hosts(id),
    command TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'accepted' CHECK (state IN ('accepted', 'started', 'progress', 'completed', 'failed', 'aborted', 'timed_out')),
    params JSONB NOT NULL DEFAULT '{}',
    issued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    acked_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    UNIQUE (tenant_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS roz_capability_leases (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES roz_hosts(id),
    resource TEXT NOT NULL,
    holder_id TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    released_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS roz_safety_policies (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    policy_json JSONB NOT NULL DEFAULT '{}',
    limits JSONB NOT NULL DEFAULT '{}',
    geofences JSONB NOT NULL DEFAULT '[]',
    interlocks JSONB NOT NULL DEFAULT '[]',
    deadman_timers JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name, version)
);

CREATE TABLE IF NOT EXISTS roz_run_provenance (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_run_id UUID NOT NULL REFERENCES roz_task_runs(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    model_id TEXT,
    model_version TEXT,
    prompt_hash TEXT,
    tool_versions JSONB NOT NULL DEFAULT '{}',
    firmware_sha TEXT,
    calibration_hash TEXT,
    sim_image TEXT,
    environment_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_frame_contracts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    host_id UUID NOT NULL REFERENCES roz_hosts(id),
    stream_name TEXT NOT NULL,
    frame JSONB NOT NULL,
    units JSONB NOT NULL DEFAULT '[]',
    transform_to_canonical JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, host_id, stream_name)
);

CREATE TABLE IF NOT EXISTS roz_degradation_modes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    environment_id UUID NOT NULL REFERENCES roz_environments(id),
    mode_name TEXT NOT NULL,
    blocked_capabilities TEXT[] NOT NULL DEFAULT '{}',
    active_capabilities TEXT[] NOT NULL DEFAULT '{}',
    alert_channels TEXT[] NOT NULL DEFAULT '{}',
    auto_transition_rules JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, environment_id, mode_name)
);

-- RLS policies
ALTER TABLE roz_commands ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_capability_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_safety_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_run_provenance ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_frame_contracts ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_degradation_modes ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_commands
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_capability_leases
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_safety_policies
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_run_provenance
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_frame_contracts
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_degradation_modes
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
