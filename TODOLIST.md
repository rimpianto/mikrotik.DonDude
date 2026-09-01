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

**Design decision (2026-09-02):** no NATS JetStream. the-other-dude needs a
message bus because its poller is a separate process fleet feeding a separate
API. DonDude is a single binary: the poller is a tokio task in `dondude serve`,
so an in-process `tokio::sync::broadcast` channel does the bus's job with zero
extra moving parts. History lives in PostgreSQL, not in the bus. If a
distributed poller is ever needed, it can be added behind the same trait
without touching the storage layer.

- [ ] 2.1 Poller task in `serve`: sample every enabled device on an interval
      (default 60 s, configurable), over the existing SSH transport; parse
      `/system resource print` (+ uptime, voltage, temperature where present).
      Writes to Postgres and publishes to the broadcast channel. One dead
      device never stops the poller; a failure resets that device's backoff.
- [ ] 2.2 `device_samples` table (migration): device_id, captured_at, cpu,
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
aware), schedule upgrades with safe-mode protection.

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
