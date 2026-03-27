-- Add progress range constraint and composite FK to enforce tenant consistency.

-- Finding 2: progress column is documented as 0.0-1.0 but has no enforced CHECK.
ALTER TABLE roz_activity_events
  ADD CONSTRAINT chk_progress_range
    CHECK (progress IS NULL OR (progress >= 0.0 AND progress <= 1.0));

-- Finding 3: single-column FK allows activity rows to have a different tenant_id
-- than their parent session. Replace with a composite FK to enforce alignment.

-- Step 1: add UNIQUE constraint on sessions so the composite FK target is valid.
ALTER TABLE roz_agent_sessions
  ADD CONSTRAINT uq_agent_sessions_id_tenant UNIQUE (id, tenant_id);

-- Step 2: drop the existing single-column FK on session_id.
ALTER TABLE roz_activity_events
  DROP CONSTRAINT roz_activity_events_session_id_fkey;

-- Step 3: add composite FK — both session_id and tenant_id must match the session.
ALTER TABLE roz_activity_events
  ADD CONSTRAINT fk_activity_events_session_tenant
    FOREIGN KEY (session_id, tenant_id)
    REFERENCES roz_agent_sessions(id, tenant_id)
    ON DELETE CASCADE;
