-- Agent sessions: metering and audit trail for gRPC agent sessions.
-- Stores session metadata (model, tokens, turns) but never message content.

CREATE TABLE IF NOT EXISTS roz_agent_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    environment_id UUID NOT NULL REFERENCES roz_environments(id) ON DELETE CASCADE,
    model_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'completed', 'cancelled', 'error')),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at TIMESTAMPTZ,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    turn_count INT NOT NULL DEFAULT 0,
    compaction_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE roz_agent_sessions ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_agent_sessions
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

CREATE INDEX idx_agent_sessions_tenant ON roz_agent_sessions(tenant_id);
CREATE INDEX idx_agent_sessions_started ON roz_agent_sessions(started_at);
