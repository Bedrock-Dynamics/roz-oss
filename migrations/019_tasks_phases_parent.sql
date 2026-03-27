-- Add phases JSONB and parent_task_id to roz_tasks for multi-phase and sub-agent support.

ALTER TABLE roz_tasks
    ADD COLUMN IF NOT EXISTS phases JSONB NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS parent_task_id UUID REFERENCES roz_tasks(id) ON DELETE SET NULL;
