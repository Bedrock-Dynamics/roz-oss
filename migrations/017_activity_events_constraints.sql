-- Add CHECK constraints to roz_activity_events to bound model-generated text columns.
ALTER TABLE roz_activity_events
  ADD CONSTRAINT chk_state CHECK (state IS NULL OR state IN ('thinking', 'calling_tool', 'idle', 'waiting_approval')),
  ADD CONSTRAINT chk_level CHECK (level IS NULL OR level IN ('full', 'mini', 'hidden')),
  ADD CONSTRAINT chk_detail_length CHECK (detail IS NULL OR length(detail) <= 512),
  ADD CONSTRAINT chk_reason_length CHECK (reason IS NULL OR length(reason) <= 512);
