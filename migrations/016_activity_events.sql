-- Activity events table for granular agent presence/activity analytics
CREATE TABLE IF NOT EXISTS roz_activity_events (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  session_id uuid NOT NULL REFERENCES roz_agent_sessions(id) ON DELETE CASCADE,
  tenant_id uuid NOT NULL,
  event_type text NOT NULL CHECK (event_type IN ('presence_hint', 'activity_update')),
  state text,        -- 'thinking', 'calling_tool', 'idle', 'waiting_approval'
  detail text,       -- tool name, status message, etc.
  level text,        -- 'full', 'mini', 'hidden' (for presence_hint events)
  reason text,       -- reason for presence change
  progress real,     -- optional progress 0.0-1.0 (for activity_update)
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Index for time-series queries per session
CREATE INDEX IF NOT EXISTS idx_activity_events_session_time
  ON roz_activity_events (session_id, created_at);

-- Index for aggregate queries by tenant
CREATE INDEX IF NOT EXISTS idx_activity_events_tenant_time
  ON roz_activity_events (tenant_id, created_at);

-- Index for filtering by event type
CREATE INDEX IF NOT EXISTS idx_activity_events_type
  ON roz_activity_events (event_type, created_at);

-- Enable RLS
ALTER TABLE roz_activity_events ENABLE ROW LEVEL SECURITY;

-- Tenant isolation policy (same pattern as roz_agent_sessions)
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
    WHERE tablename = 'roz_activity_events' AND policyname = 'tenant_isolation'
  ) THEN
    CREATE POLICY tenant_isolation ON roz_activity_events
      FOR ALL
      USING (tenant_id = current_setting('rls.tenant_id', true)::uuid)
      WITH CHECK (tenant_id = current_setting('rls.tenant_id', true)::uuid);
  END IF;
END $$;
