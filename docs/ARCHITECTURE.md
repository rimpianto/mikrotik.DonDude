# Architecture

Design notes for anyone working on DonDude: the invariants that are easy to
break, the reasons behind choices that look odd, and where each concern lives.

## What this is

DonDude is a multi-tenant MikroTik RouterOS fleet manager with a web interface (a
Rust rewrite of *the-other-dude*). Phase 1 — the only implemented phase —
captures RouterOS `/export` configurations over SSH and versions them in a
**separate** backup Git repository. Later phases (state monitoring, SNMP,
safe-mode config pushes with rollback, firmware management, SRP-6a auth) are not
written yet; the module boundaries exist to accommodate them.

The package is `mikrotik-dondude`; the binary is **`dondude`**. It ships as a
Docker image driven by `docker-compose.yml` (app + PostgreSQL).

**There is no configuration file.** The database is the single source of truth:
devices, credentials, GitHub settings and the schedule are all rows, edited in
the browser. Only deployment settings come from the environment
(`DATABASE_URL`, `DONDUDE_MASTER_KEY`, `DONDUDE_REPO_PATH`, `DONDUDE_BIND`).
Anything that reintroduces a second place to configure the same thing is a
regression — that duplication is what this design exists to avoid.

## Commands

```sh
cargo build
cargo test                      # 50 tests; no router, database or network needed
cargo test --lib                # unit tests only
cargo test normalized_output    # single test by substring
cargo test --test pipeline      # one integration target
cargo fmt && cargo fmt --check
cargo clippy --all-targets      # kept warning-free
cargo run -- keygen

# The SQL layer, against a throwaway PostgreSQL:
docker run -d --name dondude-pg -e POSTGRES_PASSWORD=dondude \
    -e POSTGRES_USER=dondude -e POSTGRES_DB=dondude -p 55432:5432 postgres:17-alpine
TEST_DATABASE_URL=postgres://dondude:dondude@127.0.0.1:55432/dondude \
    cargo test --test database

# The whole stack:
docker compose build && docker compose up -d
docker compose logs -f app
docker compose exec app dondude fleet list
```

`tests/database.rs` **skips itself** when `TEST_DATABASE_URL` is unset, so plain
`cargo test` passes anywhere. Keep it that way; do not make the default test run
depend on a live database.

### Native dependencies

libgit2, libssh2 and OpenSSL are **vendored** (`vendored-libgit2`,
`vendored-openssl`), so no `pkg-config` or system `-dev` packages are needed —
but a C compiler, `cmake` and `perl` are, and the first build takes several
minutes. Do not "fix" a slow first build by switching to system libraries; this
was chosen deliberately because the build host has no `pkg-config`.

Consequence for the runtime image: vendored OpenSSL has no certificate path that
matches Debian's, so the `Dockerfile` sets `SSL_CERT_FILE`/`SSL_CERT_DIR`.
Without them, pushing to GitHub fails with a certificate error.

## Architecture

```
                    ┌─ web/     axum + maud, server-rendered HTML
PostgreSQL ─▶ db ─▶ ┤
   (truth)          └─ main.rs  CLI (serve, backup run, device test, …)
                         │
                         ▼
                     backup.rs  (orchestrator)
                         ├─▶ routeros/   SSH + /export, normalized
                         └─▶ git/        diff detect, commit, push
```

Dependencies point one way and should stay that way:

* `routeros` knows nothing about Git; `git` knows nothing about RouterOS;
  `backup.rs` is the only module that knows both.
* `config.rs` holds plain runtime types with **no SQL and no serde** — it is what
  the engine runs on. `db::runtime_config` assembles it from rows and decrypts
  credentials on the way out. So the engine has no database dependency and tests
  can build a fleet in three lines.
* `web/` never touches `routeros` or `git` credentials directly; it goes through
  `db` and `backup`.

`src/lib.rs` is the library; `src/main.rs` is a thin clap CLI.

### The two repositories

This checkout is the **source** repo. The `.rsc` files go to a **backup** repo at
`DONDUDE_REPO_PATH` (`/data/backups` in the container) with its own GitHub
remote — never here. `BackupRepo::open_or_init` refuses a path containing
`Cargo.toml` for exactly this reason; keep that guard.

### The central invariant: diff stability

A raw `/export` begins with a banner carrying the device's current clock:

```
# 2024-01-15 10:22:31 by RouterOS 7.13.2
```

Committed verbatim, every device produces a diff on every run and the history
becomes worthless. `routeros/export.rs` parses that banner for facts worth
keeping and rewrites it *without* the timestamp, while deliberately keeping the
firmware version (an upgrade is a real change and belongs in the diff). Only the
leading contiguous comment block is stripped, so `#` inside script bodies
survives.

**Anything volatile that reaches the stored file re-breaks this.** The tests
`normalized_output_is_stable_across_captures` and
`re_running_an_unchanged_device_produces_exactly_one_commit` are the guards.

Change detection compares against the working-tree file *and* `git status` for
that path — bytes alone would miss a run killed between write and commit, which
leaves the tree permanently ahead of `HEAD`.

### Secrets

`crypto.rs` seals router passwords, key passphrases and the GitHub token with
XChaCha20-Poly1305 under `DONDUDE_MASTER_KEY`, which lives outside the database.
The process refuses to start without the key; a silent fallback to plaintext
would be the worst outcome, because nothing would look broken.

* Only `db` and `crypto` ever see ciphertext. Row types handed to the web layer
  carry `has_secret: bool`, never the secret, so a template cannot render one.
* `DeviceAuth`, `GitAuth`, `Device` and `Remote` have **hand-written `Debug`**
  that redacts. Do not derive `Debug` on them — `debug_output_never_contains_a_secret`
  and the database test both check this.
* Operator logins are Argon2id hashes; session cookies are stored as SHA-256
  digests of a 256-bit random token.
* An empty secret field in a form means "keep what is stored". That rule lives in
  `db::update_device` / `db::update_settings`, not in the handlers.

### Concurrency and the blocking boundary

`ssh2`/libssh2 is synchronous, so one device's entire conversation happens inside
a single `spawn_blocking` (`routeros::capture`). Concurrency is many such tasks
bounded by a `Semaphore`. libgit2 is blocking too, so `AppState::open_repo` and
`probe_remote` also go through `spawn_blocking` rather than stalling an HTTP
worker.

Git work is serial: captures are pipelined through `buffer_unordered` and folded
into the repository as each lands, giving one commit per device in a
deterministic order without a barrier at the end.

Two things in `backup::run` look odd and must not be "simplified":

* The capture futures are built with a plain iterator and collected into a `Vec`
  before `stream::iter(...).buffer_unordered(...)`. Handing a closure to
  `StreamExt::map` there needs a higher-ranked lifetime it cannot have, and the
  run future then cannot be spawned (`FnOnce is not general enough`).
* `progress` is a generic `&P: ProgressSink + ?Sized`, not `&dyn ProgressSink`,
  for the same reason.

`RunManager` allows **one run at a time**; two would race on the Git index and
interleave commits. The slot is claimed before the run row is created, and
released if that insert fails.

### Error policy

`error.rs` splits errors in two, and the split is load-bearing:

* `DeviceError` — one device. Recorded in the report; the run continues. One dead
  router must never cost the rest of the fleet its backup.
* `Error` — configuration, repository, database, environment. Aborts the run.

Variants with a `#[source]` leave the cause out of their own `Display` text;
`anyhow`'s `{:#}` and `error::chain()` walk the chain. Adding `{source}` back
into a format string prints every cause twice.

## Conventions and gotchas

* **Schema changes**: `migrations/` is embedded with `sqlx::migrate!` and applied
  automatically by `dondude serve`. Add a new migration file rather than editing
  the initial one once anything is deployed. `Db` row structs mirror columns:
  Postgres `INTEGER` is `i32`, `BIGINT` is `i64`, `TEXT[]` is `Vec<String>`.
* **sqlx 0.9** feature names are `runtime-tokio` + `tls-rustls-ring` (not the 0.8
  `runtime-tokio-rustls`). Queries use the runtime API, never `query!`, so
  compiling never needs a live `DATABASE_URL`. Keep it that way.
* **axum 0.8** path parameters are `{id}`, not `:id`. Private pages are enforced
  by extracting `Operator`; a handler that does not ask for one has no route to
  an operator's data, so authorization cannot be forgotten.
* **All user-facing strings live in `web/views.rs`** (English), including the CSS
  and the only piece of JavaScript in the project — a dozen lines polling
  `/api/runs/{id}`. There is no JS build step; do not add one for a form.
* **Flash messages are fixed codes** (`?ok=saved`), never free text, so nothing
  user-supplied is reflected into a page. Failed form posts re-render with the
  submitted values instead of redirecting, so an operator does not retype
  everything to fix a typo.
* **Path safety**: device and tenant names come from a web form and become file
  paths. `config::slugify` flattens them and must never yield `.`, `..` or an
  empty component; `BackupRepo::resolve` rejects escapes as defence in depth.
  `config::render_backup_path` is the single implementation, shared by the engine
  and the UI so they cannot disagree about where a file lives.
* **git2 0.21 API**: `Reference::shorthand()`, `Remote::url()`, `Commit::summary()`
  and `Commit::body()` return `Result`, not `Option`. `CheckoutBuilder` lives in
  `git2::build`. A **rejected push is reported through the
  `push_update_reference` callback, not as an `Err`** — `BackupRepo::push`
  collects rejections and fails on them, otherwise a non-fast-forward push looks
  like success.
* **Tenant isolation belongs in PostgreSQL**, via row-level security keyed on the
  transaction-local `dondude.tenant_id` setting (`Db::set_tenant`), not in Rust
  filtering. UUIDs are generated application-side so the schema needs no
  `pgcrypto` and no superuser. Note the app currently owns its tables, so it is
  exempt from RLS until it runs as a non-owning role.
* **Host keys** are verified by default (`accept-new`, matching OpenSSH). `HOME`
  points at `/data` in the image so `known_hosts` survives restarts. Keep
  `strict`/`accept-new`/`off` meaningful; `HostKeyPolicy::parse` falls back to
  `accept-new`, never to `off`.
* `show_sensitive` defaults to **false** — a backup repo is a softer target than
  the routers it describes. Don't flip the default.
* **The scheduler** wakes every 30s and asks the *database* whether a scheduled
  run already started in the last five minutes. Tracking that in memory would
  re-fire after every restart. Times are UTC on purpose.
