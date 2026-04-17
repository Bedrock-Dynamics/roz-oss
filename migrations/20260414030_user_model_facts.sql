-- Phase 17 MEM-03: per-tenant dialectic user-model facts.
--
-- Composite PK symmetric with D-01: (tenant_id, observed_peer_id,
-- observer_peer_id, fact_id). peer_id columns are TEXT (not UUID) because
-- they may be CLI usernames, external federation ids, or UUIDs depending
-- on session origin — they are opaque identifiers, not FKs.
--
-- Dedup index on md5(fact) per (tenant, observed_peer_id) supports D-07
-- exact-match dedup without pulling in pgvector.
-- Stale sweep index supports TTL-based filtering at query time.

CREATE TABLE IF NOT EXISTS roz_user_model_facts (
    tenant_id         UUID        NOT NULL REFERENCES roz_tenants(id) ON DELETE CASCADE,
    observed_peer_id  TEXT        NOT NULL,
    observer_peer_id  TEXT        NOT NULL,
    fact_id           UUID        NOT NULL DEFAULT gen_random_uuid(),
    fact              TEXT        NOT NULL CHECK (char_length(fact) > 0 AND char_length(fact) <= 1024),
    source_turn_id    UUID        NULL REFERENCES roz_session_turns(id) ON DELETE SET NULL,
    confidence        REAL        NOT NULL DEFAULT 0.7 CHECK (confidence >= 0.0 AND confidence <= 1.0),
    stale_after       TIMESTAMPTZ NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, observed_peer_id, observer_peer_id, fact_id)
);

CREATE INDEX roz_user_model_facts_lookup
    ON roz_user_model_facts (tenant_id, observed_peer_id, observer_peer_id, created_at DESC);

CREATE INDEX roz_user_model_facts_dedup
    ON roz_user_model_facts (tenant_id, observed_peer_id, md5(fact));

CREATE INDEX roz_user_model_facts_stale
    ON roz_user_model_facts (stale_after) WHERE stale_after IS NOT NULL;

ALTER TABLE roz_user_model_facts ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON roz_user_model_facts
    USING (tenant_id = current_setting('rls.tenant_id')::uuid)
    WITH CHECK (tenant_id = current_setting('rls.tenant_id')::uuid);
