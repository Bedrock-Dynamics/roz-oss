-- Message feedback: stores user thumbs-up/down ratings on agent messages.
-- Used for quality monitoring and RLHF training data.

CREATE TABLE IF NOT EXISTS roz_message_feedback (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES roz_agent_sessions(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    rating TEXT NOT NULL CHECK (rating IN ('up', 'down')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (session_id, message_id)
);

ALTER TABLE roz_message_feedback ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_message_feedback
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

CREATE INDEX idx_message_feedback_session ON roz_message_feedback(session_id);
