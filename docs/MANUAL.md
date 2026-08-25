# Manual

Reference for running DonDude. If you are setting it up for the first time,
start with [GETTING-STARTED.md](GETTING-STARTED.md) instead.

- [What DonDude does](#what-dondude-does)
- [The interface](#the-interface)
- [Settings, field by field](#settings-field-by-field)
- [Devices](#devices)
- [Runs](#runs)
- [The command line](#the-command-line)
- [Deployment](#deployment)
- [Security](#security)
- [Troubleshooting](#troubleshooting)
- [Backup and restore](#backup-and-restore)

---

## What DonDude does

For each device in the fleet, on demand or on a schedule:

1. connects over SSH
2. runs `/export` and reads `/system` metadata (identity, version, model, serial)
3. normalizes the output so only real changes produce a diff
4. compares it against the file already committed
5. commits it if it changed, one commit per device
6. pushes once, after the whole fleet has been walked

A device that cannot be reached is recorded as failed and the run continues.
One dead router never costs the rest of the fleet its backup.

### Why the export is rewritten

A raw `/export` begins with a banner carrying the device's current clock:

```
# 2024-01-15 10:22:31 by RouterOS 7.13.2
```

Committed as-is, every device would produce a diff on every run and the history
would say nothing about what actually changed. DonDude parses that banner for
the facts worth keeping and rewrites it without the timestamp:

```
# RouterOS configuration export captured by DonDude
# device = core-rtr-01
# identity = core
# model = RB5009UG+S+
# serial number = HGT08XXXXX
# routeros = 7.13.2
# command = /export terse
#
```

The firmware version is deliberately kept: an upgrade is a real change and
belongs in the diff. The capture time is not lost — it goes into the commit,
where it does not cause churn.

Only the leading run of comment lines is replaced, so `#` characters inside
script bodies survive untouched.

### What the repository looks like

```
acme/core-rtr-01.rsc
acme/edge-rtr-02.rsc
lab/chr-01.rsc
```

One commit per device per change, so `git log -- acme/core-rtr-01.rsc` is that
router's real change history:

```
backup(core-rtr-01): +3 -1 lines [RouterOS 7.14.3]

Device: core-rtr-01
Host: 10.0.0.1
Tenant: acme
Identity: core
RouterOS: 7.14.3
Model: RB5009UG+S+
Serial: HGT08XXXXX
Command: /export terse
Captured: 2026-08-25T02:30:04Z
```

Commit timestamps mirror the capture, so `git log` reads as a timeline of the
fleet rather than of the machine running DonDude.

---

## The interface

| Page | What it is for |
|---|---|
| **Dashboard** | Fleet at a glance, last result per device, start a backup or a dry run |
| **Devices** | Add, edit, enable, delete; test a connection; back up one device |
| **Device → history** | Every commit that changed that router, with a coloured diff |
| **Runs** | Every run, its live log while it happens, per-device outcomes |
| **Settings** | Repository and token, capture options, host-key policy, schedule |

The version of the running build is in the top bar and the footer of every page,
so you never have to guess what is deployed.

### Dry run

Connects to every device and reports what *would* change — no writes, no
commits, no push. Use it on a fleet you have just added, or to check reachability
without touching history.

---

## Settings, field by field

### GitHub repository

| Field | Notes |
|---|---|
| **Repository URL** | HTTPS URL of a **private** repository. Empty keeps backups on this machine only. |
| **Branch** | `main` unless you have a reason. |
| **Username** | Ignored by GitHub when the password is a token; `x-access-token` is the convention. |
| **Access token** | Encrypted before storage and never shown again. Empty on save keeps the stored one; a single `-` removes it. |
| **Push after each run** | Off keeps history local while still committing. |

**Save and test connection** stores the settings and then connects, so what is
on screen is what was stored and what was tested. It proves read access; write
access is only proven by a real push.

> Configure the repository **before the first backup** if you can. Committing
> locally first and adding the repository afterwards leaves two unrelated
> histories — see [unrelated histories](#unrelated-histories) below.

### Capture

| Field | Notes |
|---|---|
| **Export detail** | `terse` (recommended) puts one command per line, so a one-setting change reads as a one-line diff. `compact` is RouterOS's default wrapped form. `verbose` includes every property, including defaults — much larger files. |
| **Host key policy** | `accept-new` records a device's key on first connection and refuses a later change, matching OpenSSH. `strict` requires the key to be known in advance. `off` disables verification — lab use only. |
| **Include secrets in exports** | Off by default. On, `/export show-sensitive` writes PSKs, PPP passwords and SNMP communities into Git in clear text. The repository is a softer target than the routers it describes. Requires the `sensitive` policy on the RouterOS user. |

### Schedule

A daily run at a fixed time, in **UTC** so it does not move with daylight
saving. The scheduler asks the database whether a scheduled run already started
recently, so a restart cannot make it fire twice.

### Advanced

| Field | Notes |
|---|---|
| **Parallel devices** | How many devices are contacted at once. Captures are parallel; commits are always serial. |
| **Connect timeout** | TCP connect budget, per device. |
| **Command timeout** | Budget for one command, including a whole `/export`. Raise it for large configurations over slow links. |
| **File layout** | Placeholders `{tenant}`, `{device}`, `{host}`. Must contain `{device}`, or devices would overwrite each other. |
| **Commit author** | Name and email on the commits DonDude creates. |
| **Repository path** | Shown for reference. Set by the deployment (a mounted volume), not editable here — where a fleet's history lives is not a browser preference. |

---

## Devices

| Field | Notes |
|---|---|
| **Name** | Unique and stable. Becomes the file name, so renaming moves the history path. |
| **Host / port** | How DonDude reaches it. |
| **SSH username** | A read-only RouterOS account is enough: `policy=ssh,read`. |
| **Tenant** | Grouping; becomes a folder in the repository. Created on first mention. |
| **Tags** | Comma separated. Lets a run cover part of the fleet. |
| **Method** | Password, an SSH private key, or a key held by `ssh-agent`. |
| **Password / passphrase** | Encrypted before storage. Leave empty when editing to keep the stored one. |
| **Private key path** | A path *inside the container*; mount the key as a volume. If a sibling `.pub` file exists it is used automatically. |
| **Include in fleet-wide runs** | Unticking excludes it from scheduled and fleet-wide runs. It can still be backed up by name. |

Names are flattened into safe file names — a device called `../../etc/passwd`
becomes `etc-passwd.rsc`, not a path escape.

**Test connection** logs in, reads identity and firmware, and exports nothing.
It also records what it learned, so the device page fills in.

**Back up now** on a device backs up that one device even if it is disabled.

Deleting a device removes it from the inventory and its run history from the
database. **Its `.rsc` file and Git history are kept** — the backup outlives the
inventory entry.

---

## Runs

Only **one run at a time**, enforced by a PostgreSQL advisory lock. Two
overlapping runs would race on the Git index and interleave commits, so whoever
asks second is told to wait. The lock covers the scheduler, the browser and the
command line alike — including a `dondude backup run` from cron on another
machine pointed at the same database.

While a run is in flight the page polls for progress. Once it ends, the page
shows the per-device table from the database. A run interrupted by a restart is
closed out as failed on the next start-up rather than sitting at "running"
forever.

### Outcomes

| Outcome | Meaning |
|---|---|
| **committed** | The configuration changed and was committed |
| **unchanged** | Byte-identical to what is already committed |
| **would change** | Dry run only: this device would have been committed |
| **failed** | Could not be captured. The previous backup is untouched. |

A run exits non-zero if any device failed or the push failed.

### Push behaviour

Before walking the fleet, DonDude fetches the remote branch and fast-forwards
onto it if it is behind, so several installations can share one backup
repository. A fetch failure is reported and the run continues — captures are
worth committing locally even when the network is down, and the next run pushes
them.

A rejected push is reported as a failure, not swallowed. libgit2 reports
rejections through a callback rather than as an error, which is easy to get
wrong; DonDude collects them and fails.

---

## The command line

The same binary, reading the same database. There is no configuration file.

```sh
docker compose exec app dondude fleet list
docker compose exec app dondude device test core-rtr-01
docker compose exec app dondude backup run
docker compose exec app dondude backup run --dry-run
docker compose exec app dondude backup run --device core-rtr-01
docker compose exec app dondude backup run --tag core --tag edge
docker compose exec app dondude backup run --tenant acme
docker compose exec app dondude backup run --concurrency 32 --no-push
docker compose exec app dondude user list
docker compose exec app dondude user add operator --password '...'
docker compose exec app dondude user passwd admin --password '...'
docker compose exec app dondude db check
docker compose exec app dondude db migrate
docker compose exec app dondude keygen
dondude --version
```

`--device` includes a device even if it is disabled. Naming one that does not
exist is an error, not an empty run — a typo should not look like a clean night.

Locked yourself out of the interface? `dondude user passwd` is the way back in.

---

## Deployment

### Environment

Only deployment facts come from the environment; everything an operator changes
lives in the database.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | — | PostgreSQL DSN. Required. |
| `DONDUDE_MASTER_KEY` | — | Base64 key encrypting stored credentials. Required. |
| `DONDUDE_REPO_PATH` | `/data/backups` | Working tree of the backup repository |
| `DONDUDE_BIND` | `0.0.0.0:8080` | Listen address |
| `DONDUDE_DB_POOL` | `5` | Database connection pool size |
| `RUST_LOG` | `mikrotik_dondude=info,warn` | Log filter |

`dondude serve` applies pending migrations on start-up, so a container comes up
ready.

### Volumes

`/data` holds the backup working tree and `known_hosts`. `HOME` points at it so
host-key pinning survives restarts.

### Behind a reverse proxy

Keep the port bound to localhost and let the proxy reach it there. Session
cookies are **not** marked `Secure`, because DonDude is commonly reached over
plain HTTP on a management LAN and a `Secure` cookie would silently never be
sent — sign-in would appear broken with no clue why. So terminate TLS in front
and redirect HTTP to HTTPS, and the cookie never travels in the clear.

Client addresses are read from `X-Forwarded-For` when present. Make sure the
proxy overwrites that header rather than passing a client-supplied one through.

### Docker inside an LXC container

Proxmox LXC needs nesting for Docker to run:

```sh
pct set <VMID> -features nesting=1,keyctl=1
pct reboot <VMID>
```

An LXC is already a container, so running the binary directly is also
reasonable: the native libraries are compiled in, so it needs only glibc and
`ca-certificates`.

---

## Security

**Credentials at rest.** Router passwords, key passphrases and the GitHub token
are sealed with XChaCha20-Poly1305 under `DONDUDE_MASTER_KEY`, which lives
outside the database. A `pg_dump` or a stolen volume snapshot leaks nothing
usable on its own. The process refuses to start without the key rather than
silently falling back to plaintext.

**Operator accounts.** Argon2id password hashes. Session cookies are 256 random
bits, stored only as a SHA-256 digest, so a database leak cannot be replayed as
a live session. Cookies are `HttpOnly` and `SameSite=Lax`, which is what
protects the state-changing forms from cross-site posts.

**Sign-in throttling.** Ten failed attempts for one username within fifteen
minutes lock that username until the window passes; thirty from one client
address do the same, which slows a spray across many usernames. Counters live in
the database, so a restart cannot reset a lockout. The trade-off is real: anyone
who can reach the form can lock a known username out for the window. The
address limit is best-effort behind a proxy — the per-username limit is the one
that has to hold.

**Secrets never render.** Row types handed to the templates carry
`has_secret: bool`, not the secret. Types holding credentials have hand-written
`Debug` implementations that redact, so nothing leaks through a log line.

**On the routers.** A read-only account (`policy=ssh,read`) with `address=`
restricted to the network DonDude runs on. Exports omit secrets by default.

**What is not covered.** There is no audit log of configuration changes made in
the interface, no per-operator permissions (every account is an administrator),
and no two-factor authentication.

---

## Troubleshooting

### Device messages

| Message | Cause |
|---|---|
| `cannot reach <host>:22: Connection refused` | SSH service disabled on the router, or the wrong port |
| `cannot reach <host>:22: Connection timed out` | A firewall in between, or `address=` on the RouterOS user excludes DonDude |
| `cannot reach <host>: hostname resolved to no addresses` | DNS failure inside the container |
| `authentication failed for user X (tried: password)` | Wrong password, or the RouterOS group is missing the `ssh` policy |
| `host key rejected: recorded key ... does not match` | The device was reinstalled or replaced. Remove its line from `known_hosts` on the `/data` volume. |
| `host key rejected: <host> is not in <file> and host_key_policy is "strict"` | Populate `known_hosts` first, or use `accept-new` |
| `'/export terse' produced no output` | Not a RouterOS device, or the user lacks `read` |
| `timed out after ...` | Raise **Command timeout**; a large export over a slow link needs headroom |
| `private key ... is unreadable` | The path is inside the container — check the volume mount |

### Remote messages

| Message | Cause |
|---|---|
| `the remote rejected every credential offered` | Wrong, expired, or under-scoped token. It needs **Contents: Read and write** on *that* repository. |
| `remote rejected the push (non-fast-forward)` | Someone else pushed. Run again — DonDude fetches and fast-forwards first. |
| `certificate verify failed` | Missing CA bundle. The image sets `SSL_CERT_FILE`; a hand-rolled deployment must too. |
| `has uncommitted changes and is behind` | Something edited the working tree by hand. Commit or discard it. |

### Unrelated histories

```
the backup repository /data/backups and origin/main have unrelated histories —
neither contains the other's commits.
```

This happens when backups were committed locally before the repository was
configured, and the repository already had a commit of its own (a README, for
example). Each side has its own root commit, so there is nothing to fast-forward
onto and DonDude refuses to guess.

Pick one, then run again:

```sh
# Keep the local backup history, replaying it on top of the remote.
git -C /data/backups rebase --onto origin/main --root

# Or adopt the remote and let the next run re-capture. Loses the local commit
# history, not the configurations — they are read from the devices again.
git -C /data/backups fetch origin
git -C /data/backups reset --hard origin/main
```

Inside Docker: `docker compose exec app git -C /data/backups ...`

To avoid it entirely, configure the repository before the first backup.

### The run will not start

`a backup run is already in progress` means the advisory lock is held — by the
scheduler, another browser tab, or a `dondude backup run` elsewhere. It clears
when that run ends, or immediately if its process dies.

### Every run produces a commit for an unchanged device

That should be impossible and is worth reporting. It means something volatile is
reaching the stored file. Compare two consecutive versions:

```sh
docker compose exec app git -C /data/backups log --oneline -- <tenant>/<device>.rsc
docker compose exec app git -C /data/backups diff HEAD~1 HEAD -- <tenant>/<device>.rsc
```

Whatever differs is the culprit.

### Lost the master key

The stored router passwords and token cannot be recovered. Generate a new key,
put it in `.env`, restart, then re-enter each device's password and the token.
Everything else — the inventory, the run history, the Git history — is intact.

---

## Backup and restore

Two things matter, and they should not live in the same place:

1. **The Git repository.** If pushing to GitHub, that *is* your off-site copy.
   Otherwise back up the `/data` volume.
2. **`DONDUDE_MASTER_KEY`.** Keep it in a password manager, not only on the
   server. Without it the credentials in the database are unreadable.

The database itself holds the inventory, settings and run history. Worth backing
up for convenience, but everything in it except the credentials can be rebuilt
by hand:

```sh
docker compose exec db pg_dump -U dondude dondude > dondude.sql
```

To restore on a new machine: bring up the stack with the **same**
`DONDUDE_MASTER_KEY`, restore the dump, and let the next run fetch the backup
repository from GitHub.
