-- Safety audit log (append-only)

CREATE TABLE IF NOT EXISTS roz_safety_audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical', 'emergency')),
    source TEXT NOT NULL,
    details JSONB NOT NULL DEFAULT '{}',
    host_id UUID,
    task_id UUID,
    policy_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Append-only: revoke UPDATE and DELETE
REVOKE UPDATE, DELETE ON roz_safety_audit_log FROM PUBLIC;

-- Index for time-range queries
CREATE INDEX idx_safety_audit_created_at ON roz_safety_audit_log (created_at DESC);
CREATE INDEX idx_safety_audit_tenant_severity ON roz_safety_audit_log (tenant_id, severity);

-- RLS
ALTER TABLE roz_safety_audit_log ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_safety_audit_log
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
