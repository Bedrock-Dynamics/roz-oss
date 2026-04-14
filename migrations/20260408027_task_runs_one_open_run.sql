-- Enforce at most one unfinished task run per task to eliminate TOCTOU race
-- in ensure_active_run (CodeRabbit fix #9).
--
-- Before creating the partial unique index, close any pre-existing duplicates
-- so the constraint applies cleanly. Keep the most-recently-started run open
-- per task; mark older open runs as 'cancelled' with a breadcrumb message.

WITH ranked AS (
    SELECT id,
           task_id,
           row_number() OVER (PARTITION BY task_id ORDER BY started_at DESC, id DESC) AS rn
    FROM roz_task_runs
    WHERE completed_at IS NULL
)
UPDATE roz_task_runs r
SET status = 'cancelled',
    completed_at = now(),
    error_message = COALESCE(r.error_message, 'auto-closed by one-open-run migration')
FROM ranked
WHERE r.id = ranked.id
  AND ranked.rn > 1;

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_runs_one_open_per_task
    ON roz_task_runs (task_id)
    WHERE completed_at IS NULL;
