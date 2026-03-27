-- Device authorization codes for RFC 8628 device flow.
-- No RLS — these rows are looked up by device_code/user_code,
-- and are ephemeral (10 min TTL).

CREATE TABLE IF NOT EXISTS roz_device_codes (
    device_code  TEXT PRIMARY KEY,
    user_code    TEXT NOT NULL UNIQUE,
    user_id      TEXT,
    tenant_id    UUID,
    expires_at   TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_device_codes_user_code ON roz_device_codes (user_code);
CREATE INDEX IF NOT EXISTS idx_device_codes_expires_at ON roz_device_codes (expires_at);
