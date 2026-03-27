-- API keys for programmatic access

CREATE TABLE IF NOT EXISTS roz_api_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_prefix TEXT NOT NULL,
    key_hash TEXT NOT NULL,
    scopes TEXT[] NOT NULL DEFAULT '{}',
    created_by TEXT NOT NULL,
    revoked_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_api_keys_key_prefix ON roz_api_keys (key_prefix);
CREATE INDEX idx_api_keys_tenant ON roz_api_keys (tenant_id);

-- RLS
ALTER TABLE roz_api_keys ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_api_keys
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
