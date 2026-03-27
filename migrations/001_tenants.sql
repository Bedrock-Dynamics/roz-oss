-- Tenancy & auth tables

CREATE TABLE IF NOT EXISTS roz_tenants (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL DEFAULT 'personal' CHECK (kind IN ('personal', 'organization')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roz_tenant_members (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'viewer' CHECK (role IN ('owner', 'admin', 'safety_officer', 'developer', 'operator', 'viewer')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, user_id)
);

-- RLS
ALTER TABLE roz_tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_tenant_members ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_tenants
    USING (id = (SELECT current_setting('rls.tenant_id', true))::uuid);

CREATE POLICY tenant_isolation ON roz_tenant_members
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
