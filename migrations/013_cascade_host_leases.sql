-- Fix: capability leases should cascade on host deletion.
-- Previously, deleting a host with released leases would fail with FK violation.
ALTER TABLE roz_capability_leases DROP CONSTRAINT roz_capability_leases_host_id_fkey;
ALTER TABLE roz_capability_leases ADD CONSTRAINT roz_capability_leases_host_id_fkey
    FOREIGN KEY (host_id) REFERENCES roz_hosts(id) ON DELETE CASCADE;
