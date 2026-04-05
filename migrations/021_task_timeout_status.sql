-- Allow explicit timeout terminal states for task lifecycle tracking.

ALTER TABLE roz_tasks DROP CONSTRAINT IF EXISTS roz_tasks_status_check;
ALTER TABLE roz_tasks
    ADD CONSTRAINT roz_tasks_status_check
    CHECK (status IN ('pending', 'queued', 'provisioning', 'running', 'succeeded', 'failed', 'timed_out', 'cancelled', 'safety_stop', 'retrying'));

ALTER TABLE roz_task_runs DROP CONSTRAINT IF EXISTS roz_task_runs_status_check;
ALTER TABLE roz_task_runs
    ADD CONSTRAINT roz_task_runs_status_check
    CHECK (status IN ('running', 'succeeded', 'failed', 'timed_out', 'cancelled', 'safety_stop'));
