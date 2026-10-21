-- Phase 5.1: board identity facts and per-device upgrade policy.

-- `architecture` joins `model`/`firmware`/`serial` as an observed fact,
-- refreshed by probes ("test device") and backup runs. It drives the Phase 5
-- firmware logic (CHR/x86 boards have no routerboard firmware).

-- `update_channel` is the operator's per-device choice of RouterOS release
-- channel; `upgrades_enabled = false` removes the device from Phase 5
-- upgrade flows entirely (no buttons in the UI, refusal server-side).
ALTER TABLE devices ADD COLUMN architecture TEXT;
ALTER TABLE devices ADD COLUMN update_channel TEXT NOT NULL DEFAULT 'stable'
    CHECK (update_channel IN ('stable', 'long-term', 'testing', 'development'));
ALTER TABLE devices ADD COLUMN upgrades_enabled BOOLEAN NOT NULL DEFAULT true;
