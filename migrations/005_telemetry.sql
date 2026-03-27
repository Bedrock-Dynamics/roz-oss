-- Telemetry tables (partitioned by time)

CREATE TABLE IF NOT EXISTS roz_telemetry (
    id UUID DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    host_id UUID NOT NULL,
    stream_name TEXT NOT NULL,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    data JSONB NOT NULL,
    PRIMARY KEY (id, ts)
) PARTITION BY RANGE (ts);

-- Create initial partition for current month
DO $$
DECLARE
    start_date DATE := date_trunc('month', CURRENT_DATE);
    end_date DATE := date_trunc('month', CURRENT_DATE) + INTERVAL '1 month';
    partition_name TEXT := 'roz_telemetry_' || to_char(CURRENT_DATE, 'YYYY_MM');
BEGIN
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I PARTITION OF roz_telemetry FOR VALUES FROM (%L) TO (%L)',
        partition_name, start_date, end_date
    );
END $$;

-- Downsampled telemetry for cold retention
CREATE TABLE IF NOT EXISTS roz_telemetry_downsampled (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    host_id UUID NOT NULL,
    stream_name TEXT NOT NULL,
    bucket_start TIMESTAMPTZ NOT NULL,
    bucket_end TIMESTAMPTZ NOT NULL,
    sample_count INTEGER NOT NULL,
    avg_data JSONB NOT NULL,
    min_data JSONB NOT NULL,
    max_data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_telemetry_tenant_host_ts ON roz_telemetry (tenant_id, host_id, ts DESC);
CREATE INDEX idx_telemetry_downsampled_lookup ON roz_telemetry_downsampled (tenant_id, host_id, stream_name, bucket_start);

-- RLS
ALTER TABLE roz_telemetry ENABLE ROW LEVEL SECURITY;
ALTER TABLE roz_telemetry_downsampled ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON roz_telemetry
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
CREATE POLICY tenant_isolation ON roz_telemetry_downsampled
    USING (tenant_id = (SELECT current_setting('rls.tenant_id', true))::uuid);
