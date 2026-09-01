# Changelog

## [0.4.0] - 2026-09-02

### Added
- Phase 2, first slice: device-state monitoring. A background task (or
  `dondude monitor poll`) samples CPU load, memory, disk, uptime and board
  health from every enabled device over SSH and stores the history in the new
  `device_samples` table, with a configurable interval and retention window
  (Settings → Monitoring). No new moving parts: same SSH transport, same
  credentials, same concurrency limit as backups. No message bus.
- `TODOLIST.md`: the roadmap, phases and design decisions (why no NATS).

