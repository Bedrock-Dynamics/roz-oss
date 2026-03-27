-- Add NATS account credential columns to environments.
-- These store the NKey keypair for the environment's NATS account.
ALTER TABLE roz_environments
    ADD COLUMN nats_account_public_key TEXT,
    ADD COLUMN nats_account_seed_encrypted TEXT;
