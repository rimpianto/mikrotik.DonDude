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

## Phase 8 — Adaptive polling interval (power optimization)

Let the operator set how often each device is polled instead of a fixed
interval. Main goal: reduce power consumption on solar/battery-powered sites
(e.g. a boat where the router is idle most of the day). Open question to
answer before implementing: does slower polling actually save meaningful
power? The router stays on either way; savings come mainly from fewer SSH
sessions and less radio traffic. Measure first with a watt meter before
investing UI complexity.

- [ ] 8.1 Per-device poll interval (minutes), defaulting to the current
      global value; `0` = use global.
- [ ] 8.2 Activity-based auto-throttle: poll fast (e.g. 60 s) while a device
      shows changes (traffic, CPU, config diff), back off to a slow interval
      (e.g. 10 min) when idle.
- [ ] 8.3 UI: per-device interval field + "idle threshold" setting, with a
      note that this targets solar/battery deployments.

## Phase 9 — Solar power monitoring

Monitor the DC power feeding a device (solar panel + battery systems, e.g.
a boat or off-grid site) and store the readings for the day.

- [ ] 9.1 Solar power sampling: poll the solar charge controller / inverter
      (SNMP, Modbus or vendor API — device-dependent, evaluation pending)
      or read the router's own DC input voltage when the panel feeds the
      router directly; store watts over time.
- [ ] 9.2 `solar_samples` table (migration): captured_at, watts, battery
      voltage, source. Same retention job as `device_samples`.
- [ ] 9.3 Dashboard "Solar" view: watts-vs-hour-of-day graph for the current
      day, overlays for previous days, battery voltage trend.
- [ ] 9.4 Forecast: predicted solar production for the coming days from
      historical production + weather forecast (sun presence). Simple model
      first (same-weekday average + cloudiness factor from a free weather
      API); refine only if the simple model is not good enough.
- [ ] 9.5 Consumption-aware forecast: combine predicted production with the
      site's consumption profile to estimate battery state of charge and
      flag days where the site may run out of power.

## Non-goals

- **NATS / Redis / message buses.** The binary is the unit of deployment.
- **TimescaleDB.** Plain PostgreSQL tables with a retention job are enough at
  fleet sizes DonDude targets.
- **Multi-process pollers.** Revisit only with a demonstrated need.
