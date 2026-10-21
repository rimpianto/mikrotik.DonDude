# DonDude — roadmap

Phases in rough priority order. Each phase lands in small, independently
shippable slices (one feature per commit, push + version bump per step).

## Phase 1 — Git-versioned configuration backups (DONE)

Connect to each router over SSH, run `/export`, normalize the output so only
real changes show up as diffs, commit per device to a dedicated backup Git
repository, push once per run. Web UI for devices, credentials, remote and
schedule. See `README.md`.

## Phase 2 — RouterOS state monitoring

Live CPU, memory, disk and interface traffic; Server-Sent Events to the
browser. Alert rules on thresholds.

**Status (2026-10-20):** 2.1 and 2.2 shipped — `src/monitor.rs` (poll_fleet
with per-device backoff, columnar `/system resource print` parsing, voltage/
temperature where present), the `monitor` background task in `serve` with the
Settings toggles (`monitor_enabled`, `monitor_interval_secs`,
`monitor_retention_days`), the `dondude monitor` one-shot sweep, the
`20260902120000_monitoring.sql` migration and the `prune_samples` retention
job running on every poll tick. Next slice: 2.3 (SSE endpoint).

**Design decision (2026-09-02):** no NATS JetStream. the-other-dude needs a
message bus because its poller is a separate process fleet feeding a separate
API. DonDude is a single binary: the poller is a tokio task in `dondude serve`,
so an in-process `tokio::sync::broadcast` channel does the bus's job with zero
extra moving parts. History lives in PostgreSQL, not in the bus. If a
distributed poller is ever needed, it can be added behind the same trait
without touching the storage layer.

- [x] 2.1 Poller task in `serve`: sample every enabled device on an interval
      (default 60 s, configurable), over the existing SSH transport; parse
      `/system resource print` (+ uptime, voltage, temperature where present).
      Writes to Postgres and publishes to the broadcast channel. One dead
      device never stops the poller; a failure resets that device's backoff.
- [x] 2.2 `device_samples` table (migration): device_id, captured_at, cpu,
      memory_used/total, disk_used/total, uptime, extra JSONB. Retention job
      drops rows older than a configurable window (default 30 days).
- [ ] 2.3 SSE endpoint `/monitor/stream`: latest sample per device pushed to
      the browser; auto-reconnect on the client.
- [ ] 2.4 Dashboard "Monitor" view: fleet table with live CPU/memory badges,
      per-device sparkline from history.
- [ ] 2.5 Alert rules: per-device (or fleet-wide) thresholds on cpu/memory;
      a breach records an event and (later) notifies.
- [ ] 2.6 Notifications: email / webhook / Slack sinks for alert rules.

## Phase 3 — SNMP

Per-device metrics via SNMP instead of one SSH session per sample. Needs a
community/v3 credential kind on devices and a Rust SNMP crate (e.g.
`snmp2`/`hr-snmp`); evaluation pending.

## Phase 4 — Safe-mode config pushes with rollback

Apply configuration changes inside a RouterOS safe-mode session: if the device
stops answering, the router reverts the change itself. Restore from any commit
in the backup repository.

## Phase 5 — Firmware management

Track installed vs. available RouterOS versions per device (architecture
aware) and orchestrate safe, operator-driven upgrades, including the
routerboard firmware step where the hardware has one.

**Decisions (2026-10-21):**
- Channels are a per-device setting, decided by the operator in the web UI
  (default `stable`).
- The routerboard firmware step is NEVER automatic: the Firmware view shows
  current vs. upgrade firmware with a dedicated per-device "Upgrade
  firmware" button running its own mini-pipeline.
- Per-device `upgrades_enabled` toggle: when off, the upgrade/firmware
  buttons are not rendered and the pipeline refuses the device
  (`outcome=upgrades-disabled`, no device contact).
- Model guardrail: a `no_auto_reboot_models` settings list (default: the
  CCR2004-PCIe entries) refuses upgrades for matching boards
  (`outcome=model-guarded`) until the restart-hook slice lands.
- Safety story: fresh config + binary backup BEFORE anything, post-restart
  wait-up with deadline and version verification, alert email on timeout.
  No safe mode (it reverts on disconnect — wrong tool for restarts).
  Netinstall is LAN-only: recovery of a dead device is a hands-on, on-site
  matter, so every upgrade stays operator-driven.
- DonDude never downloads/stores .npk packages: the router fetches its own
  updates (architecture aware) via `/system package update`.

- [x] 5.1 Persist board identity on devices (board_name, architecture,
      ros_version, update_channel, upgrades_enabled) fed by the existing
      SystemInfo parsing; migration.
- [ ] 5.2 Update-check runner over SSH: `/system package update print`
      + `/system routerboard print` (CHR/x86 = no firmware), persisted to
      a `device_updates` table with per-device failure isolation.
- [ ] 5.3 Firmware view: fleet table (versions, channel selector, badges,
      Check now); upgrade buttons hidden for `upgrades_enabled = false`.
- [ ] 5.4 Upgrade pipelines with guard step: RouterOS pipeline
      (guard -> pre-flight backup -> download/install -> restart -> wait-up
      -> verify -> report) and the separate operator-triggered firmware
      mini-pipeline; `upgrade_runs` table; email report via notify.rs.
- [ ] 5.5 Scheduling of update CHECKS (automatic); actual upgrades always
      operator-clicked (auto-upgrade = later explicit opt-in).
- [ ] 5.6 Docs: MANUAL/README/CHANGELOG for the two-firmware model, CHR
      behavior, safety story, operator checklist.

**Deferred:** per-device restart hooks for special boards (CCR2004-PCIe
`linksafe` family) — contract sketched, blocked on the operator's script
(interface/versioning).

## Phase 6 — RouterOS binary API transport

Port 8728/8729 as a second `Transport` (already scaffolded in
`src/routeros/mod.rs`), faster and lighter than SSH for monitoring samples.

## Phase 7 — SRP-6a zero-knowledge auth

Replace password login with SRP-6a so the server never sees the operator
password, the way the-other-dude does.

## Non-goals

- **NATS / Redis / message buses.** The binary is the unit of deployment.
- **TimescaleDB.** Plain PostgreSQL tables with a retention job are enough at
  fleet sizes DonDude targets.
- **Multi-process pollers.** Revisit only with a demonstrated need.
