-- Out-of-band billing/usage schema re-homed for repo/DB parity.
--
-- These tables were applied to the dev database before 2026-04-11 via
-- operator-run SQL, not via committed migrations. They are a precondition
-- for the `record_usage()` function re-homed in migration 20260408026.
--
-- `IF NOT EXISTS` guards make this idempotent against the dev DB (which
-- already contains these objects) while producing the same end-state on a
-- fresh deployment.
--
-- Tables re-homed: roz_plan_limits, roz_billing_periods, roz_usage_events,
-- roz_usage_reservations, roz_trial_grants.

-- ---------------------------------------------------------------------------
-- roz_plan_limits — static plan definitions (free, pro, enterprise, …)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS roz_plan_limits (
    plan_slug     TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    usage_limit   NUMERIC(12, 6) NOT NULL,
    hard_limit    BOOLEAN NOT NULL DEFAULT true,
    features      JSONB NOT NULL DEFAULT '{}'::jsonb,
    rate_limits   JSONB NOT NULL DEFAULT '{}'::jsonb,
    max_devices   INTEGER,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- roz_billing_periods — per-tenant monthly billing accumulator
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS roz_billing_periods (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID NOT NULL REFERENCES roz_tenants(id),
    period_start  TIMESTAMPTZ NOT NULL,
    period_end    TIMESTAMPTZ NOT NULL,
    plan          TEXT NOT NULL DEFAULT 'free',
    total_usage   NUMERIC(12, 6) NOT NULL DEFAULT 0
                    CONSTRAINT billing_periods_usage_non_negative CHECK (total_usage >= 0),
    usage_limit   NUMERIC(12, 6) NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, period_start)
);

-- ---------------------------------------------------------------------------
-- roz_usage_events — immutable record of every billable event
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS roz_usage_events (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id              UUID NOT NULL REFERENCES roz_tenants(id),
    session_id             UUID REFERENCES roz_agent_sessions(id),
    resource_type          TEXT NOT NULL,
    model                  TEXT,
    quantity               BIGINT NOT NULL
                             CONSTRAINT usage_events_quantity_non_negative CHECK (quantity >= 0),
    input_tokens           BIGINT,
    output_tokens          BIGINT,
    cache_read_tokens      BIGINT,
    cache_creation_tokens  BIGINT,
    internal_cost          NUMERIC(12, 6) NOT NULL DEFAULT 0
                             CONSTRAINT usage_events_cost_non_negative CHECK (internal_cost >= 0),
    idempotency_key        TEXT NOT NULL UNIQUE,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_usage_events_tenant_created
    ON roz_usage_events (tenant_id, created_at);
CREATE INDEX IF NOT EXISTS idx_usage_events_resource
    ON roz_usage_events (tenant_id, resource_type, created_at);
CREATE INDEX IF NOT EXISTS idx_usage_events_session
    ON roz_usage_events (session_id);

-- ---------------------------------------------------------------------------
-- roz_usage_reservations — pre-flight reservation holds (2-phase usage)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS roz_usage_reservations (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        UUID NOT NULL REFERENCES roz_tenants(id),
    session_id       UUID,
    reserved_amount  NUMERIC(12, 6) NOT NULL
                       CONSTRAINT reservations_amount_non_negative CHECK (reserved_amount >= 0),
    status           TEXT NOT NULL DEFAULT 'pending',
    idempotency_key  TEXT NOT NULL UNIQUE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at       TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_reservations_tenant_pending
    ON roz_usage_reservations (tenant_id) WHERE (status = 'pending');

-- ---------------------------------------------------------------------------
-- roz_trial_grants — which Clerk users have redeemed a trial
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS roz_trial_grants (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    clerk_user_id  TEXT NOT NULL UNIQUE,
    tenant_id      UUID NOT NULL REFERENCES roz_tenants(id),
    granted_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_trial_grants_user
    ON roz_trial_grants (clerk_user_id);
