-- Phase 20 MCP-01/MCP-02/MCP-06: tenant-scoped server-side MCP registration.
--
-- `roz_mcp_servers` stores one row per tenant/server registration.
-- `roz_mcp_server_credentials` stores encrypted auth material behind
-- `credentials_ref`, following the Phase 19 "ciphertext + nonce" posture.
--
-- RLS matches the existing per-tenant tables: callers set
-- `SET LOCAL rls.tenant_id = '<uuid>'` inside a transaction via
-- `roz_db::set_tenant_context`.

CREATE TABLE IF NOT EXISTS roz_mcp_server_credentials (
    tenant_id                   UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    id                          UUID        NOT NULL,
    auth_kind                   TEXT        NOT NULL CHECK (auth_kind IN ('none', 'bearer', 'header', 'oauth')),
    header_name                 TEXT,
    bearer_ciphertext           BYTEA,
    bearer_nonce                BYTEA,
    header_value_ciphertext     BYTEA,
    header_value_nonce          BYTEA,
    oauth_access_ciphertext     BYTEA,
    oauth_access_nonce          BYTEA,
    oauth_refresh_ciphertext    BYTEA,
    oauth_refresh_nonce         BYTEA,
    oauth_expires_at            TIMESTAMPTZ,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, id),

    CONSTRAINT roz_mcp_server_credentials_auth_shape CHECK (
        (auth_kind = 'none'
            AND header_name IS NULL
            AND bearer_ciphertext IS NULL
            AND bearer_nonce IS NULL
            AND header_value_ciphertext IS NULL
            AND header_value_nonce IS NULL
            AND oauth_access_ciphertext IS NULL
            AND oauth_access_nonce IS NULL
            AND oauth_refresh_ciphertext IS NULL
            AND oauth_refresh_nonce IS NULL
            AND oauth_expires_at IS NULL)
     OR (auth_kind = 'bearer'
            AND bearer_ciphertext IS NOT NULL
            AND bearer_nonce IS NOT NULL
            AND header_name IS NULL
            AND header_value_ciphertext IS NULL
            AND header_value_nonce IS NULL
            AND oauth_access_ciphertext IS NULL
            AND oauth_access_nonce IS NULL
            AND oauth_refresh_ciphertext IS NULL
            AND oauth_refresh_nonce IS NULL
            AND oauth_expires_at IS NULL)
     OR (auth_kind = 'header'
            AND header_name IS NOT NULL
            AND header_value_ciphertext IS NOT NULL
            AND header_value_nonce IS NOT NULL
            AND bearer_ciphertext IS NULL
            AND bearer_nonce IS NULL
            AND oauth_access_ciphertext IS NULL
            AND oauth_access_nonce IS NULL
            AND oauth_refresh_ciphertext IS NULL
            AND oauth_refresh_nonce IS NULL
            AND oauth_expires_at IS NULL)
     OR (auth_kind = 'oauth'
            AND oauth_access_ciphertext IS NOT NULL
            AND oauth_access_nonce IS NOT NULL
            AND bearer_ciphertext IS NULL
            AND bearer_nonce IS NULL
            AND header_value_ciphertext IS NULL
            AND header_value_nonce IS NULL)
    )
);

CREATE INDEX roz_mcp_server_credentials_lookup
    ON roz_mcp_server_credentials (tenant_id, id);

CREATE OR REPLACE FUNCTION roz_mcp_server_credentials_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_mcp_server_credentials_touch_trg
    BEFORE UPDATE ON roz_mcp_server_credentials
    FOR EACH ROW EXECUTE FUNCTION roz_mcp_server_credentials_touch_updated_at();

ALTER TABLE roz_mcp_server_credentials ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_mcp_server_credentials
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);

CREATE TABLE IF NOT EXISTS roz_mcp_servers (
    tenant_id                   UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name                        TEXT        NOT NULL,
    transport                   TEXT        NOT NULL CHECK (transport IN ('streamable_http')),
    url                         TEXT        NOT NULL,
    credentials_ref             UUID,
    enabled                     BOOLEAN     NOT NULL DEFAULT true,
    failure_count               INTEGER     NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    degraded_at                 TIMESTAMPTZ,
    last_error                  TEXT,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, name),
    CONSTRAINT roz_mcp_servers_credentials_fk
        FOREIGN KEY (tenant_id, credentials_ref)
        REFERENCES roz_mcp_server_credentials (tenant_id, id)
);

CREATE INDEX roz_mcp_servers_enabled_lookup
    ON roz_mcp_servers (tenant_id, enabled, degraded_at, updated_at DESC);

CREATE OR REPLACE FUNCTION roz_mcp_servers_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_mcp_servers_touch_trg
    BEFORE UPDATE ON roz_mcp_servers
    FOR EACH ROW EXECUTE FUNCTION roz_mcp_servers_touch_updated_at();

ALTER TABLE roz_mcp_servers ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_mcp_servers
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
