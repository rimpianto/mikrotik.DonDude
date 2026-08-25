# mikrotik.DonDude

Multi-tenant management platform for MikroTik RouterOS fleets, with a web
interface. A modular Rust rewrite of *the-other-dude*.

**Phase 1 (implemented): Git-versioned configuration backups.** DonDude connects
to each router over SSH, runs `/export`, normalizes the output so only real
changes show up as diffs, commits each device's `.rsc` file to a dedicated backup
Git repository with the device name, firmware and capture time in the commit
message, and pushes once per run. Devices, credentials, the GitHub repository and
the nightly schedule are all managed in the browser.

Planned: RouterOS state monitoring, SNMP, safe-mode config pushes with automatic
rollback, firmware management, and SRP-6a zero-knowledge auth.

## Quick start

```sh
git clone https://github.com/rimpianto/mikrotik.DonDude.git
cd mikrotik.DonDude

cp .env.example .env
$EDITOR .env                                        # set POSTGRES_PASSWORD

docker compose build
docker compose run --rm --no-deps app keygen        # paste the key into .env
docker compose up -d
```

Then open <http://localhost:8080> and create the administrator account.

**New here?** [docs/GETTING-STARTED.md](docs/GETTING-STARTED.md) walks through
the whole thing — router account, GitHub token, first backup — in about twenty
minutes. [docs/MANUAL.md](docs/MANUAL.md) is the reference for every screen,
setting and error message.

The database schema is applied automatically on start-up. Nothing else is
needed: add a router in **Devices**, point **Settings** at a GitHub repository,
and press **Back up all devices now**.

### The master key

`DONDUDE_MASTER_KEY` encrypts every credential DonDude stores — the router
passwords and the GitHub token. It lives in `.env`, outside the database, so a
database dump or a stolen volume snapshot leaks nothing usable on its own.

**Keep a copy somewhere safe.** Without it the stored credentials cannot be
decrypted and every device has to be given its password again. DonDude refuses to
start without it rather than silently falling back to storing secrets in the
clear.

## What the interface does

| Page | What it is for |
|---|---|
| **Dashboard** | Fleet at a glance, last result per device, start a backup or a dry run |
| **Devices** | Add, edit, enable and delete routers; test a connection |
| **Device → history** | Every commit that changed that router, with a coloured diff |
| **Runs** | Every run, its live log while it happens, and per-device outcomes |
| **Settings** | GitHub repository and token, export detail, host-key policy, daily schedule |

A **dry run** connects to every device and reports what *would* change, without
writing, committing or pushing anything. Useful for a first look at a new fleet.

## Setting up the GitHub remote

1. Create an **empty, private** repository — it will describe your network.
2. Generate a fine-grained personal access token limited to that one repository,
   with **Contents: Read and write**. (A classic token needs the `repo` scope,
   which grants access to *all* your repositories; prefer the fine-grained one.)
3. In **Settings**, paste the repository URL and the token, then press **Test
   connection** before saving.

The token is encrypted before it is stored, and the settings page never renders
it back — it shows only whether one is present.

If the repository already has history, DonDude fetches and fast-forwards onto it
rather than forking it, so several installations can share one backup repository.

## Security posture

* **Credentials at rest.** Router passwords and the GitHub token are sealed with
  XChaCha20-Poly1305 under `DONDUDE_MASTER_KEY`, which lives outside the
  database. Operator logins are Argon2id hashes; session cookies are stored only
  as SHA-256 digests.
* **Sign-in throttling.** Ten failed attempts for one username within fifteen
  minutes lock that username until the window passes; thirty from one client
  address do the same, which slows a spray across many usernames. The address is
  taken from `X-Forwarded-For` behind a proxy and so is best-effort — the
  per-username limit is the one that has to hold. The trade-off is that anyone
  who can reach the form can lock a known username out for the window.
* **One run at a time.** A PostgreSQL advisory lock stops a scheduled run, a
  click in the browser and a `dondude backup run` from cron from interleaving
  commits in the backup repository. Whoever asks second is told to wait.
* **Session cookies are not marked `Secure`**, because DonDude is commonly
  reached over plain HTTP on a management LAN and a `Secure` cookie would
  silently never be sent, making sign-in look broken. Put TLS in front for
  anything internet-facing, and redirect HTTP to HTTPS so the cookie never
  travels in the clear.

## On the routers

A dedicated read-only account is enough:

```
/user group add name=backup policy=ssh,read
/user add name=dondude-backup group=backup password="..." address=10.0.0.0/24
```

`ssh` is needed to log in and `read` to read the configuration; nothing else is
required. Add `sensitive` only if you turn on *include secrets in exports* — which is off by default, because those values would be committed to
Git in clear text and the repository is a softer target than the routers.

Host keys are verified with an `accept-new` policy by default: the key is
recorded on first connection and a later change is refused. `known_hosts` lives
on the `/data` volume, so pinning survives restarts.

## How backups are stored

The backup repository lives on the `/data` volume, separate from this source
repository, and is laid out by tenant:

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
RouterOS: 7.14.3
Model: RB5009UG+S+
Serial: HGT08XXXXX
Command: /export terse
Captured: 2026-08-25T02:30:04Z
```

A raw `/export` begins with a banner containing the router's own clock, so
committing it verbatim would produce a diff for every device on every run.
DonDude rewrites that banner without the timestamp but *keeps* the firmware
version — an upgrade is a real change and belongs in the diff. Unchanged devices
produce no write and no commit.

## Command line

The same binary also works from a terminal, reading the same database, which is
what makes it usable from cron or a script:

```sh
docker compose exec app dondude fleet list
docker compose exec app dondude device test core-rtr-01
docker compose exec app dondude backup run --dry-run
docker compose exec app dondude backup run --tag core
docker compose exec app dondude user add operator --password '...'
docker compose exec app dondude db check
```

`backup run` exits non-zero if any device or the push failed. There is no
configuration file: the database is the single source of truth, so the CLI and
the UI can never disagree.

## Building without Docker

```sh
cargo build --release          # binary: target/release/dondude
```

libgit2, libssh2 and OpenSSL are vendored and built from source, so no
`pkg-config` or system `-dev` packages are needed — but a C compiler, `cmake` and
`perl` are, and the first build takes several minutes.

```sh
export DATABASE_URL=postgres://dondude:secret@localhost:5432/dondude
export DONDUDE_MASTER_KEY=$(dondude keygen)
export DONDUDE_REPO_PATH=/var/lib/dondude/backups
dondude serve
```

## Tests

```sh
cargo test                     # unit and integration tests; no router, database or network needed
TEST_DATABASE_URL=postgres://... cargo test          # adds the SQL and HTTP layers
```

Device behaviour is covered with canned `/export` text, and push/fetch against a
local bare repository created by the test. The PostgreSQL test is skipped unless
`TEST_DATABASE_URL` is set — and it **truncates every table**, so point it at a
throwaway database.

## Documentation

| | |
|---|---|
| [GETTING-STARTED.md](docs/GETTING-STARTED.md) | From nothing to a router backed up on GitHub |
| [MANUAL.md](docs/MANUAL.md) | Every screen and setting, the command line, troubleshooting |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it works inside, and the invariants to respect when changing it |

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
