-- Add JSONB columns for storing embodiment data on hosts.
-- Both nullable: a host can exist before embodiment data is uploaded.

ALTER TABLE roz_hosts
    ADD COLUMN embodiment_model JSONB,
    ADD COLUMN embodiment_runtime JSONB;
