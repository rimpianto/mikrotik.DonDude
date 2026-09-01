-- Phase 2: RouterOS state monitoring.

-- One row per device per successful poll. Written by the monitor task (or
-- `dondude monitor poll`); read by the dashboard and, later, alerting.
--
-- Kept narrow on purpose: only the numbers the UI graphs. Anything a future
-- feature needs can go into `extra` JSONB without another migration.
CREATE TABLE device_samples (
    id           UUID PRIMARY KEY,
    device_id    UUID NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    captured_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- /system resource print
    cpu_load     INTEGER CHECK (cpu_load BETWEEN 0 AND 100),
    free_memory  BIGINT,
    total_memory BIGINT,
    free_hdd     BIGINT,
    total_hdd    BIGINT,
    -- Parsed into seconds from "1w2d3h4m5s" on the device.
    uptime_secs  BIGINT,

    -- /system health print (absent on many boards; all optional)
    voltage      DOUBLE PRECISION,
    temperature  DOUBLE PRECISION,

    -- Future-proofing: values a later slice wants without a migration.
    extra        JSONB NOT NULL DEFAULT '{}'
);

-- The dashboard reads the latest sample per device; this index serves it.
CREATE INDEX device_samples_device_time_idx ON device_samples (device_id, captured_at DESC);

-- Monitoring settings, on the single settings row like the backup schedule.
ALTER TABLE settings ADD COLUMN
    monitor_enabled        BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE settings ADD COLUMN
    monitor_interval_secs  INTEGER NOT NULL DEFAULT 60
        CHECK (monitor_interval_secs >= 10);
ALTER TABLE settings ADD COLUMN
    monitor_retention_days INTEGER NOT NULL DEFAULT 30
        CHECK (monitor_retention_days > 0);
