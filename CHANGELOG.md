# Changelog

## [0.5.4] - 2026-09-03

### Fixed
- Clippy is now clean across the whole workspace: the ignored `Result`s in
  `update now` are propagated, the dead `volts` helper is gone, and the two
  view functions that legitimately take one argument per rendered column
  carry an explained `allow(too_many_arguments)`.

### Docs
- The README screenshots were recaptured from a fresh v0.5.3 install:
  empty-state pages with placeholder data only (no real fleet names or
  addresses), current version in the header, favicon in the tab.

## [0.5.3] - 2026-09-03

### Added
- Dashboard: the header sub-line shows when the next fleet-wide monitoring
  poll is due ("next fleet poll around 10:28:50 UTC") whenever monitoring is
  enabled, mirroring the per-device line on the device page.

## [0.5.2] - 2026-09-03

### Fixed
- `dondude update now` failed at the database-dump step when `DATABASE_URL`
  was not set on the host — which is the normal case for a Compose deployment.
  The dump now runs `pg_dump` inside the `db` container over its local socket,
  so no database credentials are needed on the host.

## [0.5.1] - 2026-09-03

### Added
- Favicon: the DonDude icon served at `/favicon.ico` (baked into the binary,
  cached for a week) and linked from every page.
- Device page: the Monitoring header now announces when the next poll is due
  ("Next poll around 09:19:31 UTC"), computed from the newest sample plus the
  configured interval. The empty state says how soon the first sample arrives
  instead of leaving a new device looking broken for up to one interval.

## [0.5.0] - 2026-09-03

### Added
- `dondude update now`: the one-command upgrade path for a Docker Compose
  deployment. Dumps the database first, stashes local changes (the compose
  file is expected to be customized), pulls, rebuilds, and switches —
  stopping at the first failure so the running container is never left half
  upgraded. The documented manual ritual stays in the guide for when the CLI
  cannot be used.
- Docs: an "Upgrading a deployment" section in GETTING-STARTED covering the
  pre-upgrade database dump and the full step order.

## [0.4.7] - 2026-09-03

### Changed
- The captured `/user print detail` and `/user ssh-keys print` block in each
  `.rsc` is now prefixed line-by-line with `# REM `, so the live user/key state
  stays readable in the versioned file but is inert if the export is ever
  pasted into a router terminal or re-imported (RouterOS skips the comments).
  Blank lines inside the block pass through unchanged.

## [0.4.6] - 2026-09-03

### Fixed
- Binary-backup diagnostics: the "not found" log now distinguishes the cause —
  a refused download (ssh2 -13) points at the missing `ftp` policy in the
  user's group, a missing file (ssh2 -28) points at the `DailyBinaryBackup`
  scheduler commands — instead of always suggesting the scheduler.
- The binary-backup help block on the device page renders each RouterOS
  command in its own copyable block with a Copy button (clipboard API), and
  includes the `/user group set ... policy=ssh,ftp,read,sensitive` command
  for the permission case.

## [0.4.5] - 2026-09-03

### Added
- Deployment backup via the web UI: a Settings card with a download button
  serving the same encrypted `.dud` archive `dondude db backup` writes
  (database dump + `.env` + `known_hosts`), assembled in memory so nothing
  touches disk inside the container. Restores unchanged with
  `dondude db restore <file>`.

### Changed
- `BackupInput::write_archive` now delegates to a new in-memory
  `archive_bytes`; the `.env`/`known_hosts` discovery helpers moved into
  `backup_archive` so the CLI and the web route share one implementation.

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

