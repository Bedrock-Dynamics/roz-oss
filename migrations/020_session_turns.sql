-- Session conversation turns for resume support.

CREATE TABLE IF NOT EXISTS roz_session_turns (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES roz_agent_sessions(id) ON DELETE CASCADE,
    turn_index INTEGER NOT NULL,
    role TEXT NOT NULL,
    content JSONB NOT NULL,
    token_usage JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(session_id, turn_index)
);

ALTER TABLE roz_session_turns ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_session_turns
    USING (session_id IN (SELECT id FROM roz_agent_sessions WHERE tenant_id = current_setting('rls.tenant_id')::uuid));
