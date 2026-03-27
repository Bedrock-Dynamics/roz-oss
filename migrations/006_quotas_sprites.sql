-- Resource quotas and sprite checkpoints

CREATE TABLE IF NOT EXISTS roz_resource_quotas (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    resource_type TEXT NOT NULL,
    max_value BIGINT NOT NULL,
    current_value BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, resource_type)
);

CREATE TABLE IF NOT EXISTS roz_sprite_checkpoints (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    environment_id UUID NOT NULL REFERENCES roz_environments(id),
    name TEXT NOT NULL,
    image_ref TEXT NOT NULL,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    metadata JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);

-- RLS
ALTER TABLE roz_resource_quotas ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_sprite_checkpoints ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_resource_quotas
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_sprite_checkpoints
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
