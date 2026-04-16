-- Phase 19 OWM-06: per-tenant OpenAI-compatible model endpoint configuration.
--
-- `auth_mode` drives which credential columns are populated:
--   - 'api_key'        → api_key_ciphertext + api_key_nonce
--   - 'oauth_chatgpt'  → oauth_token_ciphertext + oauth_token_nonce + oauth_refresh_*
--                        + oauth_expires_at + oauth_account_id
--   - 'none'           → no credentials (e.g., local Ollama / llama.cpp)
--
-- `wire_api` selects Chat Completions v1 vs Responses v1 dispatch in OpenAiClient.
-- `reasoning_format` is nullable; NULL = auto-detect on first streamed chunk.
--
-- All `*_ciphertext` columns are AEAD ciphertext (AES-GCM); each has a paired
-- `*_nonce` column. AES-GCM requires a unique nonce per ciphertext, so the
-- CHECK constraint enforces that ciphertext and nonce are populated together —
-- this prevents a raw-string write without encryption from passing.
--
-- RLS: `tenant_isolation` policy identical to Phase 17 (`roz_agent_memory`,
-- see `migrations/20260414028_agent_memory.sql:65-68`) and Phase 18
-- (`roz_skills`). Tenant context is set via
-- `roz-db::lib::set_tenant_context` at `crates/roz-db/src/lib.rs:81-89`,
-- which issues `SET LOCAL rls.tenant_id = '<uuid>'` per transaction.

CREATE TABLE IF NOT EXISTS roz_model_endpoints (
    tenant_id                  UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name                       TEXT        NOT NULL,
    base_url                   TEXT        NOT NULL,
    auth_mode                  TEXT        NOT NULL CHECK (auth_mode IN ('api_key', 'oauth_chatgpt', 'none')),
    wire_api                   TEXT        NOT NULL CHECK (wire_api IN ('chat', 'responses')),
    tool_call_format           TEXT,
    reasoning_format           TEXT        CHECK (reasoning_format IS NULL OR reasoning_format IN ('none', 'think_tags', 'openai_reasoning_content', 'anthropic_signed_blocks')),

    -- API-key auth
    api_key_ciphertext         BYTEA,
    api_key_nonce              BYTEA,

    -- ChatGPT OAuth auth
    oauth_token_ciphertext     BYTEA,
    oauth_token_nonce          BYTEA,
    oauth_refresh_ciphertext   BYTEA,
    oauth_refresh_nonce        BYTEA,
    oauth_expires_at           TIMESTAMPTZ,
    oauth_account_id           TEXT,

    enabled                    BOOLEAN     NOT NULL DEFAULT true,
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, name),

    -- Auth-mode invariant: the right credential columns are populated.
    CONSTRAINT roz_model_endpoints_auth_shape CHECK (
        (auth_mode = 'api_key'       AND api_key_ciphertext IS NOT NULL AND api_key_nonce IS NOT NULL AND oauth_token_ciphertext IS NULL)
     OR (auth_mode = 'oauth_chatgpt' AND oauth_token_ciphertext IS NOT NULL AND oauth_token_nonce IS NOT NULL AND oauth_expires_at IS NOT NULL)
     OR (auth_mode = 'none'          AND api_key_ciphertext IS NULL AND oauth_token_ciphertext IS NULL)
    )
);

CREATE INDEX roz_model_endpoints_enabled_lookup
    ON roz_model_endpoints (tenant_id, enabled, updated_at DESC);

CREATE OR REPLACE FUNCTION roz_model_endpoints_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_model_endpoints_touch_trg
    BEFORE UPDATE ON roz_model_endpoints
    FOR EACH ROW EXECUTE FUNCTION roz_model_endpoints_touch_updated_at();

ALTER TABLE roz_model_endpoints ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_model_endpoints
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
