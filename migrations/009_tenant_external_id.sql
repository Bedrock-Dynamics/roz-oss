-- Add external_id to roz_tenants for Clerk org/user ID mapping.
-- This allows correlating Clerk webhook events and JWT claims
-- with internal tenant rows.

ALTER TABLE roz_tenants ADD COLUMN external_id TEXT UNIQUE;
