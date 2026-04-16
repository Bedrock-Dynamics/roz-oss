-- Phase 17 MEM-02: curated long-term agent/user memory.
--
-- `subject_id` defaults to a sentinel UUID (`00000000-0000-0000-0000-000000000000`)
-- representing a tenant/agent-wide entry. Postgres rejects function expressions
-- (like COALESCE) inside PRIMARY KEY constraints, so the Rust layer maps
-- `Option<Uuid>::None <-> SUBJECT_SENTINEL` to preserve D-01 semantics.
--
-- Char caps (Hermes parity): 2200 per entry (CHECK); 2200 total per (tenant,scope,subject)
-- for agent scope, 1375 total for user scope (trigger).
--
-- RLS: tenant_isolation policy identical to every v2.0 table.

CREATE TABLE IF NOT EXISTS roz_agent_memory (
    tenant_id    UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    scope        TEXT        NOT NULL CHECK (scope IN ('agent','user')),
    subject_id   UUID        NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'::uuid,
    entry_id     UUID        NOT NULL DEFAULT gen_random_uuid(),
    content      TEXT        NOT NULL,
    char_count   INTEGER     NOT NULL CHECK (char_count > 0 AND char_count <= 2200),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, scope, subject_id, entry_id)
);

CREATE INDEX roz_agent_memory_lookup
    ON roz_agent_memory (tenant_id, scope, subject_id, updated_at DESC);

-- Per-(tenant,scope,subject) total-char cap enforced via BEFORE INSERT OR UPDATE.
-- Agent scope: 2200 chars total; user scope: 1375 chars total.
CREATE OR REPLACE FUNCTION roz_agent_memory_total_char_cap() RETURNS trigger AS $$
DECLARE
    cap INTEGER;
    current_total INTEGER;
BEGIN
    cap := CASE NEW.scope WHEN 'agent' THEN 2200 WHEN 'user' THEN 1375 ELSE 2200 END;
    SELECT COALESCE(SUM(char_count), 0) INTO current_total
      FROM roz_agent_memory
     WHERE tenant_id = NEW.tenant_id
       AND scope = NEW.scope
       AND subject_id = NEW.subject_id
       AND entry_id <> NEW.entry_id;
    IF current_total + NEW.char_count > cap THEN
        RAISE EXCEPTION 'memory scope % exceeds char cap (% + % > %)',
            NEW.scope, current_total, NEW.char_count, cap;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_agent_memory_char_cap_trg
    BEFORE INSERT OR UPDATE ON roz_agent_memory
    FOR EACH ROW EXECUTE FUNCTION roz_agent_memory_total_char_cap();

CREATE OR REPLACE FUNCTION roz_agent_memory_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_agent_memory_touch_trg
    BEFORE UPDATE ON roz_agent_memory
    FOR EACH ROW EXECUTE FUNCTION roz_agent_memory_touch_updated_at();

ALTER TABLE roz_agent_memory ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_agent_memory
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
