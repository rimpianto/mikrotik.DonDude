-- Phase 5.2: the last update check per device.

-- One row per device (UNIQUE device_id, upserted): what RouterOS build the
-- device runs versus what its channel offers, and which routerboard firmware
-- is flashed versus what the installed packages ship. CHR/x86 devices have
-- no routerboard firmware (firmware_supported = false). A failed check
-- keeps the previous versions and only records `last_error`.
CREATE TABLE device_updates (
    id                 UUID PRIMARY KEY,
    device_id          UUID NOT NULL UNIQUE REFERENCES devices (id) ON DELETE CASCADE,
    checked_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    channel           TEXT,
    installed_version TEXT,
    latest_version    TEXT,
    status            TEXT,
    fw_current        TEXT,
    fw_upgrade        TEXT,
    firmware_supported BOOLEAN,
    last_error        TEXT
);
