# READMEFIRST.md

Welcome to DonDude. This page is the map: pick the path that matches who you
are, and follow the links.

**DonDude is a multi-tenant management platform for MikroTik RouterOS
fleets, with a web interface.** It backs up router configurations to your own
Git repository and monitors device health. It is free software (GPL-3.0+).

There are two ways to run it. Almost everyone wants **Path A**.

---

## Path A — I just want to run DonDude (no development)

You do not need Rust, a compiler, or the source code. DonDude ships as:

1. **A container image** (recommended — includes the web UI, the database
   runs alongside it):

   ```sh
   docker pull ghcr.io/rimpianto/mikrotik.dondude:latest
   ```

   Tags exist for `latest`, `0.4` and `0.4.2` — pick a specific version for
   a deployment. Works on both amd64 and arm64 machines. Set up the stack by
   following the [Quick start](README.md#quick-start) in the README (use the
   image above instead of `docker compose build`).

2. **Standalone Linux binaries** (if you don't want Docker at all):

   Download `dondude-vX.Y.Z-<arch>.tar.gz` for your architecture
   (`x86_64` or `aarch64`) from the
   [releases page](https://github.com/rimpianto/mikrotik.DonDude/releases),
   together with the matching `.sha256` file. Then:

   ```sh
   sha256sum -c dondude-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.sha256
   tar xzf dondude-*.tar.gz
   ```

   You provide your own PostgreSQL and point DonDude at it with
   `DATABASE_URL`. See the [manual](docs/MANUAL.md) for every setting.

Either way, when a new version comes out, upgrade = download the new
image/binary. Your data lives in the database and in the backup repository,
not in the application.

Once it runs: [Getting started](docs/GETTING-STARTED.md) walks you through
the router account, the Git remote and the first backup — about twenty
minutes. The [manual](docs/MANUAL.md) documents every screen, setting and
command.

---

## Path B — I want to work on the source code

DonDude is written in Rust and uses Docker Compose for the development
stack. To build it yourself:

1. Install Rust (stable) — <https://rustup.rs>
2. Install Docker with the Compose plugin
3. Clone and run:

   ```sh
   git clone https://github.com/rimpianto/mikrotik.DonDude.git
   cd mikrotik.DonDude
   cp .env.example .env
   $EDITOR .env        # set POSTGRES_PASSWORD
   docker compose build
   docker compose run --rm --no-deps app keygen   # paste the key into .env
   docker compose up -d
   ```

The [architecture document](docs/ARCHITECTURE.md) explains how it works
inside and the invariants to respect when changing it. Tests, migrations and
conventions are described there too.

---

## Where to ask things

* [Discussions](https://github.com/rimpianto/mikrotik.DonDude/discussions) —
  questions, ideas, "is this a bug?"
* [Issues](https://github.com/rimpianto/mikrotik.DonDude/issues) — actual
  bugs, with steps to reproduce
