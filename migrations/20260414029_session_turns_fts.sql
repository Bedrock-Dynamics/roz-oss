-- Phase 17 MEM-01 + MEM-06: full-text search over session turns, and a 'kind'
-- column to distinguish normal turns from rolling-compaction summary turns.
--
-- Requires Postgres 14+ for `jsonb_path_query_array`. Hosted PG targets
-- (Supabase/Neon/fly.io-managed) all ship 14+ as of 2026-04-14.
--
-- `content_tsv` is GENERATED ALWAYS AS STORED so Rust never writes it and
-- it's indexed directly. The extractor flattens all nested `.text` fields
-- (assistant/user message parts, tool_use input, tool_result content).
-- See pitfall 2: a bare `content::text` cast would tokenize JSON punctuation.

ALTER TABLE roz_session_turns
    ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'turn'
        CHECK (kind IN ('turn','compaction'));

ALTER TABLE roz_session_turns
    ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (
        to_tsvector('english', coalesce(jsonb_path_query_array(content, '$.**.text')::text, ''))
    ) STORED;

CREATE INDEX IF NOT EXISTS roz_session_turns_content_tsv_gin
    ON roz_session_turns USING GIN (content_tsv);

CREATE INDEX IF NOT EXISTS roz_session_turns_kind_idx
    ON roz_session_turns (session_id, kind) WHERE kind <> 'turn';
