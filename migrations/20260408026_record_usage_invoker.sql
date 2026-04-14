-- Out-of-band migration re-homed for repo/DB parity.
--
-- This function was applied to the dev database on 2026-04-11 via an
-- operator-run SQL session, not via a committed migration. It has
-- version 20260408026 in `_sqlx_migrations` and a null checksum, which
-- means sqlx skips content verification — but still requires a file
-- with the matching version prefix to exist, or server startup fails
-- with "migration 20260408026 was previously applied but is missing in
-- the resolved migrations".
--
-- This file restores parity. `CREATE OR REPLACE` is safe on re-apply
-- and matches the exact function signature currently in the dev DB.
--
-- The referenced billing/usage tables (roz_usage_events, roz_billing_periods,
-- roz_plan_limits) are pre-existing in the dev DB and orthogonal to
-- Phase 16.1 — they arrived via the same out-of-band path and are not
-- in scope here.

CREATE OR REPLACE FUNCTION public.record_usage(
    p_tenant_id uuid,
    p_session_id uuid,
    p_resource_type text,
    p_model text,
    p_quantity bigint,
    p_input_tokens bigint,
    p_output_tokens bigint,
    p_cache_read_tokens bigint,
    p_cache_creation_tokens bigint,
    p_internal_cost numeric,
    p_idempotency_key text
)
RETURNS void
LANGUAGE plpgsql
AS $function$
BEGIN
    INSERT INTO roz_usage_events (
        tenant_id, session_id, resource_type, model, quantity,
        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
        internal_cost, idempotency_key
    ) VALUES (
        p_tenant_id, p_session_id, p_resource_type, p_model, p_quantity,
        p_input_tokens, p_output_tokens, p_cache_read_tokens, p_cache_creation_tokens,
        p_internal_cost, p_idempotency_key
    ) ON CONFLICT (idempotency_key) DO NOTHING;

    IF FOUND THEN
        INSERT INTO roz_billing_periods (
            tenant_id, period_start, period_end, plan, usage_limit, total_usage
        )
        SELECT
            p_tenant_id,
            date_trunc('month', now()),
            date_trunc('month', now()) + interval '1 month',
            t.plan,
            COALESCE(pl.usage_limit, 5.00),
            p_internal_cost
        FROM roz_tenants t
        LEFT JOIN roz_plan_limits pl ON pl.plan_slug = t.plan
        WHERE t.id = p_tenant_id
        ON CONFLICT (tenant_id, period_start) DO UPDATE SET
            total_usage = roz_billing_periods.total_usage + p_internal_cost,
            plan = EXCLUDED.plan,
            usage_limit = EXCLUDED.usage_limit,
            updated_at = now();
    END IF;
END;
$function$;
