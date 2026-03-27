-- migrations/008_rls_with_check.sql
-- Add WITH CHECK clauses to all existing RLS policies
-- Required for INSERT operations to respect tenant isolation
--
-- IMPORTANT: All existing policies are named `tenant_isolation` (verified in migrations 001-007).
-- The ::uuid cast is required because current_setting() returns TEXT but columns are UUID.

-- Tenants (uses `id` not `tenant_id`)
ALTER POLICY tenant_isolation ON roz_tenants
    USING (id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_tenant_members
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Core primitives (migration 002)
ALTER POLICY tenant_isolation ON roz_environments
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_hosts
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_tasks
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_task_runs
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_triggers
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_skills
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_skill_versions
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_streams
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Control primitives (migration 003)
ALTER POLICY tenant_isolation ON roz_commands
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_capability_leases
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_safety_policies
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_run_provenance
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_frame_contracts
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_degradation_modes
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Safety audit (migration 004 — INSERT-only, UPDATE/DELETE already revoked)
ALTER POLICY tenant_isolation ON roz_safety_audit_log
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Telemetry (migration 005)
ALTER POLICY tenant_isolation ON roz_telemetry
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_telemetry_downsampled
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- Quotas & sprites (migration 006)
ALTER POLICY tenant_isolation ON roz_resource_quotas
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

ALTER POLICY tenant_isolation ON roz_sprite_checkpoints
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);

-- API keys (migration 007)
ALTER POLICY tenant_isolation ON roz_api_keys
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid)
    WITH CHECK (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
