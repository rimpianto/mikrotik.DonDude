# Getting started

From nothing to a router backed up on GitHub. Allow about twenty minutes, most
of it waiting for the first Docker build.

Everything here is done once. After it, backups happen on their own.

---

## Before you start

You need:

* a machine with Docker and Docker Compose
* a MikroTik device reachable over SSH from that machine
* a GitHub account

---

## 1. Get DonDude running

```sh
git clone https://github.com/rimpianto/mikrotik.DonDude.git
cd mikrotik.DonDude
cp .env.example .env
```

Open `.env` and set `POSTGRES_PASSWORD` to any strong random string. It is only
used between the two containers, so you will never type it again.

Build the image. The first build compiles libgit2, libssh2 and OpenSSL from
source and takes several minutes; later builds are fast.

```sh
docker compose build
```

Now generate the key that encrypts stored credentials, and put it in `.env`:

```sh
docker compose run --rm --no-deps app keygen
```

Copy the line it prints into `DONDUDE_MASTER_KEY=` in `.env`.

> **Keep a copy of this key somewhere safe** — a password manager, not only the
> server. It decrypts every router password and the GitHub token. Without it
> those are unrecoverable and every device has to be given its password again.
> A database backup and this key stored in the same place defeats half the point
> of encrypting them.

Start it:

```sh
docker compose up -d
docker compose logs -f app          # Ctrl-C to stop watching
```

You should see `DonDude is listening on http://0.0.0.0:8080`. The database
schema is applied automatically.

Open <http://localhost:8080>. It asks you to create the administrator account —
username, and a password of at least 8 characters. That account is stored
hashed with Argon2id; there is no recovery link, but you can reset it from the
command line later.

**Before upgrading an existing installation** (a rebuild or a new image), take
a backup first — one encrypted file with the whole database:

```sh
docker compose run --rm --no-deps app db backup /data/backups
```

Keep the resulting `.dud` file and your `.env` (it holds the master key)
somewhere safe; `dondude db restore` brings everything back on a fresh stack.

---

## 2. Create a user on the router

DonDude only ever reads. Give it an account that can do nothing else:

```
/user group add name=backup policy=ssh,read,ftp,sensitive
/user add name=dondude-backup group=backup password="pick-something-long" \
    address=192.168.1.0/24
```

What each policy is for:

| Policy | Needed for |
|---|---|
| `ssh` | Logging in and running commands |
| `read` | Reading the configuration |
| `ftp` | File transfer over SSH (SFTP/SCP): downloading the daily `.backup` file. **Not** the FTP TCP service — that can stay disabled in `/ip service`. |
| `sensitive` | Reading `.backup` files (they contain passwords) and producing `show-sensitive` exports |

Nothing else is needed — not `write`, not `test`.

Set `address=` to the network DonDude runs on, so the account is useless from
anywhere else.

### Optional: an SSH key instead of the password

Instead of (or alongside) the password, the `dondude-backup` account can accept
a public key — no shared secret to rotate, and nothing to type. Generate a
dedicated pair if you do not have one:

```sh
ssh-keygen -t ed25519 -f ~/.ssh/dondude_ed25519 -N ""
```

Then import the public half on the router (RouterOS 7):

```
/user ssh-keys import user=dondude-backup public-key-file=dondude_ed25519.pub
```

Upload the `.pub` file first (drag it into Winbox's *Files*, or
`scp ~/.ssh/dondude_ed25519.pub admin@192.168.1.1:`). The key works only for
the user it was imported for; the private half never leaves the DonDude host.
In DonDude, leave the password field empty and point the device at the key file.

Check that SSH is enabled:

```
/ip service print
```

`ssh` must not be `disabled`. The `ftp` service does **not** need to be
enabled: on RouterOS the file system is served by the FTP subsystem, but the
SFTP/SCP file transfer runs inside SSH.

### The daily binary backup (optional but recommended)

DonDude also downloads a full binary backup — passwords and MAC addresses
included — if the router produces one daily:

```
/system scheduler add name=DailyBinaryBackup interval=1d start-time=03:00:00 \
    on-event="/system backup save name=AutomatedBinaryBackup dont-encrypt=yes"
```

If the file is missing, DonDude logs a warning with this very command and the
run still succeeds. Store `.rsc` and `.backup` together: the `.rsc` is the
auditable, diffable history; the `.backup` is what you restore in a
disaster-recovery scenario.

> The `sensitive` policy is what allows reading the `.backup` file. Without it
> the export still works, but the binary download is refused with
> *Permission denied* and reported as "not found".

---

## 3. Add the router in DonDude

**Devices → Add device**

| Field | What to put |
|---|---|
| Name | Short and stable, e.g. `core-rtr-01`. It becomes the file name in Git, so renaming it later moves its history. |
| Host or IP | How DonDude reaches it |
| SSH port | 22 unless you changed it |
| SSH username | `dondude-backup` |
| Tenant | A grouping, e.g. a customer or site. Becomes a folder in the repository. `default` is fine. |
| Tags | Optional labels for backing up part of the fleet, e.g. `core, milan` |
| Method | Password |
| Password | The one you just set on the router |

Save, then press **Test connection**.

A green banner like `Connected: RB5009UG+S+, RouterOS 7.14.3` means credentials
and reachability are fine. The device's host key is recorded now, on this first
connection, and a later change will be refused — that is the `accept-new` policy,
the same one OpenSSH uses.

If it fails, the message says why. The table at the end of
[MANUAL.md](MANUAL.md) lists what each one means.

---

## 4. Set up the backup repository

Do this **before the first backup**. Backing up locally first and adding the
repository afterwards leaves two unrelated histories that need a git command to
untangle.

Any Git host that takes a token works — GitHub, or a Gitea you run yourself.
Create the repository **private**: it will describe your network. Empty is
cleanest, but one initialised with a README works too.

### With GitHub

*Settings → Developer settings → Personal access tokens → Fine-grained tokens →
Generate new token*

* **Repository access**: `Only select repositories` → the backup repository only
* **Permissions → Repository permissions → Contents: Read and write**

GitHub adds *Metadata: Read-only* by itself; that is expected. Nothing else is
needed. A classic token would need the `repo` scope, which grants access to
*all* your repositories — prefer the fine-grained one.

In DonDude — **Settings**:

* **Repository URL**: `https://github.com/you/mikrotik-backups.git`
* **Branch**: `main`
* **Username**: leave `x-access-token` — GitHub ignores it when the password is
  a token
* **Access token**: paste it

### With a self-hosted Gitea

*Settings → Applications → Access tokens*, with the `write:repository` scope.

In DonDude — **Settings**:

* **Repository URL**: `http://gitea.lan:3000/you/mikrotik-backups.git`
* **Branch**: `main`
* **Username**: **your Gitea account name** — unlike GitHub, Gitea checks it
* **Access token**: paste it

Plain `http://` on a LAN needs nothing else. If your Gitea is on `https://` with
a self-signed certificate, tick **Accept an untrusted TLS certificate** — and
read [what that gives up](MANUAL.md#self-hosted-instances-with-a-self-signed-certificate)
first.

Press **Save and test connection**. Expect one of:

```
Settings saved. Connected. The repository is empty; the first push will create `main`.
Settings saved. Connected. Branch `main` exists (1 branch(es) total).
```

The token is encrypted before it is stored and is never shown again — the field
will say `stored — type to replace`.

---

## 5. The first backup

From the dashboard, press **Dry run** first. It connects to every device and
reports what *would* change, without writing, committing or pushing anything.
Nothing can go wrong, and you find out whether the fleet is reachable.

Then press **Back up all devices now** and watch the log:

```
19:50:32 1 device(s) selected; repository /data/backups
19:50:32 remote: the remote branch does not exist yet
19:50:34 core-rtr-01: committed — initial, 226 lines home/core-rtr-01.rsc
19:50:35 pushed to the backup remote
19:50:35 1 device(s): 1 changed, 0 unchanged, 0 failed in 3.1s
```

Refresh your GitHub repository — the `.rsc` file is there.

### The test that proves it works

Press **Back up all devices now** again, straight away.

It must report **unchanged**, and create **no** new commit.

That is the whole point of the design. A raw `/export` starts with a banner
carrying the router's own clock, so committing it as-is would produce a diff for
every device on every run and the history would tell you nothing. DonDude
rewrites that banner without the timestamp — while keeping the firmware version,
because an upgrade *is* a real change.

If you see a second commit instead, something volatile is reaching the stored
file. That is a bug worth reporting.

---

## 6. Make it automatic

**Settings → Schedule** → tick *Back up automatically every day* and pick a time.

Times are **UTC**, deliberately: a scheduled run does not shift twice a year
with daylight saving.

That is it. Nothing else to set up — no cron, no systemd timer.

---

## The CLI on the host (for Compose deployments)

Inside a Docker Compose deployment the `dondude` binary lives only in the app
container, which is right for commands that work on the database
(`docker compose exec app dondude db check`). But commands that manage the
*deployment itself* — above all `update now` — need `git`, the `docker` CLI
and the project checkout, which live on the host, not in the container.

So install the release binary once on the host:

```sh
cd /tmp
curl -sSL -o dondude.tar.gz \
  https://github.com/rimpianto/mikrotik.DonDude/releases/download/v0.5.0/dondude-v0.5.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf dondude.tar.gz
install -m 755 dondude-v0.5.0-x86_64-unknown-linux-gnu/dondude /usr/local/bin/dondude
dondude --version        # sanity check
rm -rf dondude.tar.gz dondude-v0.5.0-x86_64-unknown-linux-gnu
```

Pick the tarball matching the host architecture (aarch64 for ARM). The binary
is self-contained; updating it is just repeating the same steps with the new
version. With it on the host, the upgrade is:

```sh
dondude update now --dir /opt/mikrotik.DonDude
```

---

## Upgrading a deployment

The upgrade ritual, in one command:

```sh
dondude update now --dir /opt/mikrotik.DonDude
```

It automates the exact steps below, in the same order, and stops at the first
failure — the running container keeps serving until the very last step, so a
broken build costs nothing but time.

The ritual it performs (worth knowing by hand):

1. **Dump the database first.** An upgrade can apply schema migrations, and
   those are one-way. The dump is the only way back:

   ```sh
   docker exec mikrotikdondude-db-1 pg_dump -U dondude -d dondude -Fc \
     -f /tmp/dondude-pre-upgrade.dump
   docker cp mikrotikdondude-db-1:/tmp/dondude-pre-upgrade.dump \
     ~/dondude-pre-upgrade.dump
   ```

2. **Pull the code**, keeping local changes (the compose file is commonly
   customized — port bindings, volumes):

   ```sh
   cd /opt/mikrotik.DonDude
   git stash && git pull --ff-only && git stash pop
   ```

3. **Rebuild and switch:**

   ```sh
   docker compose build app
   docker compose up -d
   ```

Migrations run automatically when the new container boots; the log line
`database schema is up to date` confirms it. The web UI also offers a
downloadable encrypted archive (Settings → *Deployment backup*) which holds the
database plus `.env` and `known_hosts` — grab one before big upgrades.

---

## Doing all of this from a script instead

Everything above except creating the first operator account can be done from the
command line, which is worth knowing before you build a second installation:

```sh
export GITHUB_TOKEN='github_pat_...'
export RTR_PASSWORD='...'

dondude user add admin --password '...'
dondude settings remote --url https://github.com/you/mikrotik-backups.git \
    --token-env GITHUB_TOKEN --push --test
dondude fleet add --update --name core-rtr-01 --host 10.0.0.1 \
    --user dondude-backup --tenant acme --password-env RTR_PASSWORD
dondude backup run --dry-run
```

`--update` makes it safe to re-run, so this doubles as the way to rebuild a
deployment without retyping anything. See
[MANUAL.md → The command line](MANUAL.md#the-command-line).

---

## Where to go next

* [MANUAL.md](MANUAL.md) — every screen and setting, the command line, and what
  to do when something fails
* [ARCHITECTURE.md](ARCHITECTURE.md) — how it works inside, and the invariants
  to respect when changing it

## Before you expose it to the internet

The interface holds the passwords to your routers. If it will be reachable from
outside a trusted network:

* Put TLS in front of it and redirect HTTP to HTTPS. Session cookies are not
  marked `Secure`, because DonDude is commonly used over plain HTTP on a
  management LAN and a `Secure` cookie would silently never be sent, making
  sign-in look broken. Behind a TLS-only proxy that is fine; over mixed HTTP it
  is not.
* Leave the compose port binding as `127.0.0.1:8080:8080` and let the proxy
  reach it locally, rather than publishing the port on every interface.

With Caddy that is three lines and you get certificates automatically:

```
dondude.example.com {
    reverse_proxy 127.0.0.1:8080
}
```
