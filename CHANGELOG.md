# Changelog

## [0.4.4] - 2026-09-02

### Fixed
- Binary-backup empty-state UX: the "none" chip on the device page claimed the
  toggle lived "in Settings", but no such setting exists — the binary backup
  is produced by the router itself, not by DonDude. The chip is now an
  expandable block that shows the exact RouterOS commands to run (the
  `DailyBinaryBackup` scheduler plus the immediate first save), rendered as a
  selectable `pre` block for one-click copy.

## [0.4.3] - 2026-09-02

### Added
- Text export now captures user and SSH-key state: the executed command is
  `/user print detail; /user ssh-keys print; /export ...`, and the user/key
  output is kept verbatim at the top of the `.rsc` between the stable header
  block and the configuration body.
- Binary backup download: after the text export, DonDude attempts to fetch
  `AutomatedBinaryBackup.backup` from the device (SFTP first, legacy SCP as
  fallback) and commits it next to the `.rsc` as its own commit. A missing file
  is a warning, never a failed run — the log tells you how to enable the daily
  scheduler on the router. Requires `ftp,sensitive` in the device's group
  policies (documented).
- Monitoring UI: the device page has a Monitoring section — a server-rendered
  SVG sparkline (CPU load and free memory in separate bands, no JavaScript)
  and a table of recent samples with used/total percentages and temperature.
- Dashboard: per-device CPU badge from the latest monitor sample, and a
  Binary column showing whether a `.backup` file is stored.
- Binary-backup status chip on the device page.

### Fixed
- Settings form did not persist the monitoring fields: enable/interval/
  retention were read but never written, so the toggle appeared to reset
  itself after saving.
- Monitor parser: `cpu-load` is a percentage (the `%` broke the parse) and
  RouterOS 7 `/system health print` is a columnar table — both are now parsed,
  so CPU load and board temperature are sampled.
- Sparkline: flat series drew no line at all, and the SVG elements were
  emitted unterminated in HTML mode, which made the browser swallow every
  series after the first.

## [0.4.2] - 2026-09-02

### Added
- Release pipeline (`.github/workflows/release.yml`): pushing a `vX.Y.Z` tag
  builds signed-checksummed `dondude` binaries for linux/amd64 and
  linux/arm64 (native runners), publishes a multi-arch container image to
  GHCR (`ghcr.io/rimpianto/mikrotik.dondude`), and creates the GitHub release
  with these notes taken from this changelog.
- README shows the current version with a link to the releases page.

## [0.4.1] - 2026-09-02

### Added
- `dondude db backup` / `dondude db restore`: a whole deployment (all tables,
  `.env`, SSH `known_hosts`) as one file, sealed with `DONDUDE_MASTER_KEY`.
  Restore is transactional and asks for confirmation. First piece of the
  update mechanism: an upgrade can now require a verified backup first.

## [0.4.0] - 2026-09-02

### Added
- Phase 2, first slice: device-state monitoring. A background task (or
  `dondude monitor poll`) samples CPU load, memory, disk, uptime and board
  health from every enabled device over SSH and stores the history in the new
  `device_samples` table, with a configurable interval and retention window
  (Settings → Monitoring). No new moving parts: same SSH transport, same
  credentials, same concurrency limit as backups. No message bus.
- `TODOLIST.md`: the roadmap, phases and design decisions (why no NATS).

