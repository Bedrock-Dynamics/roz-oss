-- Phase 21 SCHED-01/SCHED-06: durable scheduled task definitions.
--
-- Stores both the original natural-language schedule and the canonical cron
-- form accepted by the server so preview/dispatch can remain deterministic.
-- Rows are tenant-scoped via the existing `rls.tenant_id` transaction setting.

CREATE TABLE IF NOT EXISTS roz_scheduled_tasks (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    name            TEXT        NOT NULL,
    nl_schedule     TEXT        NOT NULL,
    parsed_cron     TEXT        NOT NULL,
    timezone        TEXT        NOT NULL,
    task_template   JSONB       NOT NULL,
    enabled         BOOLEAN     NOT NULL DEFAULT true,
    catch_up_policy TEXT        NOT NULL CHECK (catch_up_policy IN ('skip_missed', 'run_latest', 'run_all')),
    next_fire_at    TIMESTAMPTZ,
    last_fire_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT roz_scheduled_tasks_fire_markers_order
        CHECK (last_fire_at IS NULL OR next_fire_at IS NULL OR last_fire_at <= next_fire_at)
);

CREATE INDEX roz_scheduled_tasks_tenant_updated_lookup
    ON roz_scheduled_tasks (tenant_id, updated_at DESC);

CREATE INDEX roz_scheduled_tasks_enabled_next_fire_lookup
    ON roz_scheduled_tasks (tenant_id, enabled, next_fire_at ASC NULLS LAST);

CREATE OR REPLACE FUNCTION roz_scheduled_tasks_touch_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER roz_scheduled_tasks_touch_trg
    BEFORE UPDATE ON roz_scheduled_tasks
    FOR EACH ROW EXECUTE FUNCTION roz_scheduled_tasks_touch_updated_at();

ALTER TABLE roz_scheduled_tasks ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_scheduled_tasks
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
