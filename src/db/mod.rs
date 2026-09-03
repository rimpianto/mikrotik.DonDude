//! PostgreSQL: the single source of truth for the fleet.
//!
//! There is no configuration file. The inventory, the GitHub settings and the
//! operator accounts all live here, and [`Db::runtime_config`] assembles them
//! into the [`Config`] the engine runs on — decrypting credentials on the way
//! out. Everything above this module (`routeros`, `git`, `backup`) therefore
//! stays free of SQL, and everything below the web layer stays free of HTTP.
//!
//! ## Secrets
//!
//! Router passwords, key passphrases and the GitHub token are stored sealed
//! (see [`crate::crypto`]) and never leave this module in readable form except
//! through `runtime_config`, which the engine needs. The row types exposed to
//! the web layer carry `has_secret: bool` rather than the secret itself, so a
//! template cannot render one by accident.
//!
//! ## Tenancy
//!
//! Tenant scoping is enforced by row-level security in PostgreSQL, keyed on the
//! transaction-local `dondude.tenant_id` setting ([`Db::set_tenant`]), not by
//! filtering in Rust. Application code that forgets to set it sees nothing
//! rather than seeing everything.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Row, Transaction};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::backup::{Outcome, PushReport, RunReport};
use crate::config::{
    Backup, Committer, Config, Device, DeviceAuth, Export, ExportMode, General, GitAuth,
    HostKeyPolicy, Remote,
};
use crate::crypto::MasterKey;
use crate::error::{Error, Result};

/// Embedded at compile time, so the binary can migrate its own schema.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// How long a login session stays valid.
const SESSION_TTL_DAYS: i64 = 30;

/// Advisory lock key guarding the backup repository. Arbitrary but fixed.
const RUN_LOCK_KEY: i64 = 0x0064_6F6E_6475_6465;

/// Failed logins tolerated per username within [`LOGIN_WINDOW_MINUTES`].
const LOGIN_MAX_PER_USER: i64 = 10;
/// Failed logins tolerated per client address in the same window. Higher,
/// because one address can legitimately host several operators.
const LOGIN_MAX_PER_IP: i64 = 30;
const LOGIN_WINDOW_MINUTES: i64 = 15;
/// Attempt records are kept this long for auditing, then dropped.
const LOGIN_RETENTION_DAYS: i64 = 30;

pub struct Db {
    pool: PgPool,
    key: MasterKey,
    /// Kept so [`Db::try_lock_run`] can open a connection of its own; see the
    /// note there about why a pooled one will not do.
    dsn: String,
}

/// Exclusive right to run a backup, held for the duration of one run.
///
/// The web UI already refuses to start two runs at once, but that check is
/// per-process: it cannot see a `dondude backup run` started from cron. Two
/// overlapping runs would race on the Git index, so the real gate is a
/// PostgreSQL advisory lock.
///
/// The lock rides on a connection of its own rather than a pooled one. Session
/// advisory locks are released when the session ends, so dropping this — even
/// through a panic or a `kill -9` — releases the lock. A pooled connection
/// would go back to the pool still holding it.
pub struct RunLock {
    _connection: sqlx::PgConnection,
}

impl std::fmt::Debug for RunLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunLock(held)")
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

/// A device as shown in the UI. Deliberately carries no secret.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_kind: String,
    pub has_secret: bool,
    pub private_key_path: Option<String>,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub identity: Option<String>,
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_outcome: Option<String>,
    pub last_detail: Option<String>,
}

impl DeviceRow {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Fleet-wide settings. `has_git_token` stands in for the token itself.
#[derive(Debug, Clone)]
pub struct Settings {
    pub path_template: String,
    pub committer_name: String,
    pub committer_email: String,
    pub remote_url: Option<String>,
    pub remote_branch: String,
    pub remote_push: bool,
    pub git_username: String,
    pub has_git_token: bool,
    /// Skip TLS verification on the remote — for a self-hosted instance with a
    /// self-signed certificate.
    pub allow_invalid_certs: bool,
    pub export_mode: String,
    pub show_sensitive: bool,
    pub concurrency: i32,
    pub connect_timeout_secs: i32,
    pub command_timeout_secs: i32,
    pub host_key_policy: String,
    pub schedule_enabled: bool,
    pub schedule_hour: i32,
    pub schedule_minute: i32,
    pub monitor_enabled: bool,
    pub monitor_interval_secs: i32,
    pub monitor_retention_days: i32,
    /// SMTP relay for run notifications. `None` host means "never send".
    pub smtp_host: Option<String>,
    pub smtp_port: i32,
    pub smtp_username: Option<String>,
    pub has_smtp_password: bool,
    pub notify_from: Option<String>,
    pub notify_to: Option<String>,
    /// true: only failed runs send mail; false: every scheduled run does.
    pub notify_on_failure_only: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunRow {
    pub id: Uuid,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub trigger: String,
    pub changed: i32,
    pub unchanged: i32,
    pub failed: i32,
    pub dry_run: bool,
    pub pushed: bool,
    pub push_detail: Option<String>,
    pub log: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EventRow {
    pub id: Uuid,
    pub run_id: Uuid,
    pub device_id: Uuid,
    pub device_name: String,
    pub outcome: String,
    pub commit_id: Option<String>,
    pub repo_path: String,
    pub insertions: i32,
    pub deletions: i32,
    pub firmware: Option<String>,
    pub detail: Option<String>,
    pub elapsed_ms: i64,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// A device as submitted by a form.
///
/// `secret` and `passphrase` use `None` for "leave what is stored alone", which
/// is what an edit form sends when the operator does not retype the password.
#[derive(Debug, Clone)]
pub struct DeviceInput {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub tenant: String,
    pub tags: Vec<String>,
    pub enabled: bool,
    pub auth_kind: String,
    pub secret: Option<String>,
    pub private_key_path: Option<String>,
}

impl DeviceInput {
    /// Reject nonsense before it reaches the database, so the UI can show a
    /// readable message instead of a constraint violation.
    fn validate(&self, creating: bool) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::config("the device needs a name"));
        }
        if self.host.trim().is_empty() {
            return Err(Error::config("the device needs a hostname or IP address"));
        }
        if self.username.trim().is_empty() {
            return Err(Error::config("the device needs an SSH username"));
        }
        if self.tenant.trim().is_empty() {
            return Err(Error::config("the device needs a tenant"));
        }
        match self.auth_kind.as_str() {
            "password" => {
                if creating && self.secret.as_deref().unwrap_or("").is_empty() {
                    return Err(Error::config("a password is required"));
                }
            }
            "key" => {
                if self
                    .private_key_path
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return Err(Error::config("a private key path is required"));
                }
            }
            "agent" => {}
            other => {
                return Err(Error::config(format!(
                    "unknown authentication type `{other}`"
                )));
            }
        }
        Ok(())
    }
}

impl Settings {
    /// The current settings as an update payload, for callers that want to
    /// change one field and leave the rest alone.
    ///
    /// `git_token` is `None`, which means "keep the stored one" — a partial
    /// update must never silently drop the token.
    pub fn to_input(&self) -> SettingsInput {
        SettingsInput {
            path_template: self.path_template.clone(),
            committer_name: self.committer_name.clone(),
            committer_email: self.committer_email.clone(),
            remote_url: self.remote_url.clone(),
            remote_branch: self.remote_branch.clone(),
            remote_push: self.remote_push,
            git_username: self.git_username.clone(),
            git_token: None,
            allow_invalid_certs: self.allow_invalid_certs,
            export_mode: self.export_mode.clone(),
            show_sensitive: self.show_sensitive,
            concurrency: self.concurrency,
            connect_timeout_secs: self.connect_timeout_secs,
            command_timeout_secs: self.command_timeout_secs,
            host_key_policy: self.host_key_policy.clone(),
            schedule_enabled: self.schedule_enabled,
            schedule_hour: self.schedule_hour,
            schedule_minute: self.schedule_minute,
            monitor_enabled: self.monitor_enabled,
            monitor_interval_secs: self.monitor_interval_secs,
            monitor_retention_days: self.monitor_retention_days,
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port,
            smtp_username: self.smtp_username.clone(),
            smtp_password: None,
            notify_from: self.notify_from.clone(),
            notify_to: self.notify_to.clone(),
            notify_on_failure_only: self.notify_on_failure_only,
        }
    }
}

/// Settings as submitted by the settings form.
#[derive(Debug, Clone)]
pub struct SettingsInput {
    pub path_template: String,
    pub committer_name: String,
    pub committer_email: String,
    pub remote_url: Option<String>,
    pub remote_branch: String,
    pub remote_push: bool,
    pub git_username: String,
    /// `None` leaves the stored token alone; `Some("")` clears it.
    pub git_token: Option<String>,
    pub allow_invalid_certs: bool,
    pub export_mode: String,
    pub show_sensitive: bool,
    pub concurrency: i32,
    pub connect_timeout_secs: i32,
    pub command_timeout_secs: i32,
    pub host_key_policy: String,
    pub schedule_enabled: bool,
    pub schedule_hour: i32,
    pub schedule_minute: i32,
    pub monitor_enabled: bool,
    pub monitor_interval_secs: i32,
    pub monitor_retention_days: i32,
    pub smtp_host: Option<String>,
    pub smtp_port: i32,
    pub smtp_username: Option<String>,
    /// None keeps the stored password, Some("") clears it, Some(p) replaces.
    pub smtp_password: Option<String>,
    pub notify_from: Option<String>,
    pub notify_to: Option<String>,
    pub notify_on_failure_only: bool,
}

// ---------------------------------------------------------------------------
// Connection and schema
// ---------------------------------------------------------------------------

impl Db {
    pub async fn connect(dsn: &str, max_connections: u32, key: MasterKey) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(dsn)
            .await?;
        debug!("connected to PostgreSQL");
        Ok(Self {
            pool,
            key,
            dsn: dsn.to_string(),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn key(&self) -> &MasterKey {
        &self.key
    }

    pub async fn migrate(&self) -> Result<()> {
        MIGRATIONS.run(&self.pool).await?;
        info!("database schema is up to date");
        Ok(())
    }

    pub async fn server_version(&self) -> Result<String> {
        let (version,): (String,) = sqlx::query_as("SELECT version()")
            .fetch_one(&self.pool)
            .await?;
        Ok(version)
    }

    /// Housekeeping at startup.
    ///
    /// A run marked `running` after a restart can never finish — the process
    /// that owned it is gone — so it is closed out as failed rather than left
    /// spinning forever in the UI.
    pub async fn recover_after_restart(&self) -> Result<()> {
        let orphans = sqlx::query(
            "UPDATE backup_runs
                SET status = 'failed',
                    finished_at = now(),
                    push_detail = COALESCE(push_detail, 'interrupted by a restart')
              WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        if orphans > 0 {
            warn!(orphans, "closed out runs interrupted by a restart");
        }
        sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM login_attempts WHERE at < now() - ($1 || ' days')::interval")
            .bind(LOGIN_RETENTION_DAYS.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Take the exclusive run lock, or return `None` if someone else holds it.
    ///
    /// Never blocks: a caller that cannot have the lock should say so, not queue
    /// up behind a run that might take minutes.
    pub async fn try_lock_run(&self) -> Result<Option<RunLock>> {
        use sqlx::Connection;

        let mut connection = sqlx::PgConnection::connect(&self.dsn).await?;
        let (acquired,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
            .bind(RUN_LOCK_KEY)
            .fetch_one(&mut connection)
            .await?;
        if !acquired {
            return Ok(None);
        }
        debug!("took the run lock");
        Ok(Some(RunLock {
            _connection: connection,
        }))
    }

    /// Scope a transaction to one tenant for the rest of its lifetime.
    ///
    /// The `true` argument makes the setting transaction-local: it is discarded
    /// on commit or rollback, so a pooled connection can never be handed on
    /// still pointed at a tenant.
    pub async fn set_tenant(tx: &mut Transaction<'_, Postgres>, tenant_id: Uuid) -> Result<()> {
        sqlx::query("SELECT set_config('dondude.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Operators and sessions
// ---------------------------------------------------------------------------

impl Db {
    pub async fn user_count(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        Ok(sqlx::query_as(
            "SELECT id, username, created_at, last_login_at FROM users ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn find_user(&self, username: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as(
            "SELECT id, username, created_at, last_login_at FROM users WHERE username = $1",
        )
        .bind(username.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn create_user(&self, username: &str, password: &str) -> Result<Uuid> {
        let username = username.trim();
        if username.is_empty() {
            return Err(Error::config("the username must not be empty"));
        }
        if password.chars().count() < 8 {
            return Err(Error::config(
                "the password must be at least 8 characters long",
            ));
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(username)
            .bind(crate::crypto::hash_password(password)?)
            .execute(&self.pool)
            .await
            .map_err(|error| match error {
                sqlx::Error::Database(ref e) if e.is_unique_violation() => {
                    Error::config(format!("user `{username}` already exists"))
                }
                other => other.into(),
            })?;
        info!(%username, "created operator account");
        Ok(id)
    }

    pub async fn set_password(&self, user_id: Uuid, password: &str) -> Result<()> {
        if password.chars().count() < 8 {
            return Err(Error::config(
                "the password must be at least 8 characters long",
            ));
        }
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(user_id)
            .bind(crate::crypto::hash_password(password)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Seconds an attempt must wait, or `None` if it may proceed.
    ///
    /// Checked *before* the password is verified, so a locked-out attacker costs
    /// us one indexed count rather than an Argon2 hash.
    ///
    /// Two limits, because they stop different things: the per-username one caps
    /// guesses against a known account, and the per-address one slows a spray
    /// across many usernames. The address is best-effort — behind a reverse proxy
    /// it comes from `X-Forwarded-For`, which a determined attacker can rotate —
    /// so the per-username limit is the one that actually has to hold.
    ///
    /// The trade-off is that someone who can reach the login form can lock a
    /// known username out for the window. That is the accepted cost of a hard
    /// limit; the alternative, an unbounded guess rate, is worse.
    pub async fn login_lockout(
        &self,
        username: &str,
        client_ip: Option<&str>,
    ) -> Result<Option<i64>> {
        let row = sqlx::query(
            "WITH recent AS (
                 SELECT username, client_ip, at FROM login_attempts
                  WHERE NOT succeeded
                    AND at > now() - ($3 || ' minutes')::interval
             )
             SELECT
                 (SELECT count(*) FROM recent WHERE username = $1) AS user_failures,
                 (SELECT max(at) FROM recent WHERE username = $1) AS user_last,
                 (SELECT count(*) FROM recent WHERE client_ip IS NOT DISTINCT FROM $2)
                     AS ip_failures,
                 (SELECT max(at) FROM recent WHERE client_ip IS NOT DISTINCT FROM $2)
                     AS ip_last",
        )
        .bind(username.trim())
        .bind(client_ip)
        .bind(LOGIN_WINDOW_MINUTES.to_string())
        .fetch_one(&self.pool)
        .await?;

        let user_failures: i64 = row.try_get("user_failures")?;
        let ip_failures: i64 = row.try_get("ip_failures")?;
        let user_last: Option<DateTime<Utc>> = row.try_get("user_last")?;
        let ip_last: Option<DateTime<Utc>> = row.try_get("ip_last")?;

        // Locked until the window has passed since the most recent failure, so
        // continuing to guess extends the wait rather than shortening it.
        let mut wait = None;
        if user_failures >= LOGIN_MAX_PER_USER {
            wait = remaining(user_last);
        }
        if ip_failures >= LOGIN_MAX_PER_IP {
            wait = wait.max(remaining(ip_last));
        }
        if wait.is_some() {
            warn!(
                username = %username,
                user_failures,
                ip_failures,
                "login throttled"
            );
        }
        Ok(wait)
    }

    /// Record an attempt. Success clears the username's failures, so an operator
    /// who mistypes a few times is not left locked out afterwards.
    pub async fn record_login_attempt(
        &self,
        username: &str,
        client_ip: Option<&str>,
        succeeded: bool,
    ) -> Result<()> {
        let username = username.trim();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO login_attempts (id, username, client_ip, succeeded)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(username)
        .bind(client_ip)
        .bind(succeeded)
        .execute(&mut *tx)
        .await?;

        if succeeded {
            sqlx::query("DELETE FROM login_attempts WHERE username = $1 AND NOT succeeded")
                .bind(username)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Verify a login. Returns `None` for both a wrong password and an unknown
    /// user, so the response cannot be used to enumerate accounts.
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<Option<User>> {
        let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE username = $1")
            .bind(username.trim())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let hash: String = row.try_get("password_hash")?;
        if !crate::crypto::verify_password(password, &hash) {
            return Ok(None);
        }
        let id: Uuid = row.try_get("id")?;
        sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(Some(
            sqlx::query_as(
                "SELECT id, username, created_at, last_login_at FROM users WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&self.pool)
            .await?,
        ))
    }

    /// Open a session and return the cookie value. Only its digest is stored.
    pub async fn create_session(&self, user_id: Uuid, user_agent: Option<&str>) -> Result<String> {
        let token = crate::crypto::session_token()?;
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, expires_at, user_agent)
             VALUES ($1, $2, now() + ($3 || ' days')::interval, $4)",
        )
        .bind(crate::crypto::token_digest(&token))
        .bind(user_id)
        .bind(SESSION_TTL_DAYS.to_string())
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(token)
    }

    /// The operator behind a cookie, if the session is still valid.
    pub async fn session_user(&self, token: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as(
            "SELECT u.id, u.username, u.created_at, u.last_login_at
               FROM sessions s
               JOIN users u ON u.id = s.user_id
              WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(crate::crypto::token_digest(token))
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_session(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(crate::crypto::token_digest(token))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

impl Db {
    pub async fn settings(&self) -> Result<Settings> {
        let row = sqlx::query(
            "SELECT path_template, committer_name, committer_email, remote_url, remote_branch,
                    remote_push, git_username, git_token_sealed IS NOT NULL AS has_git_token,
                    allow_invalid_certs,
                    export_mode, show_sensitive, concurrency, connect_timeout_secs,
                    command_timeout_secs, host_key_policy, schedule_enabled, schedule_hour,
                    schedule_minute, monitor_enabled, monitor_interval_secs,
                    monitor_retention_days,
                    smtp_host, smtp_port, smtp_username,
                    smtp_password_sealed IS NOT NULL AS has_smtp_password,
                    notify_from, notify_to, notify_on_failure_only
               FROM settings WHERE id",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(Settings {
            path_template: row.try_get("path_template")?,
            committer_name: row.try_get("committer_name")?,
            committer_email: row.try_get("committer_email")?,
            remote_url: row.try_get("remote_url")?,
            remote_branch: row.try_get("remote_branch")?,
            remote_push: row.try_get("remote_push")?,
            git_username: row.try_get("git_username")?,
            has_git_token: row.try_get("has_git_token")?,
            allow_invalid_certs: row.try_get("allow_invalid_certs")?,
            export_mode: row.try_get("export_mode")?,
            show_sensitive: row.try_get("show_sensitive")?,
            concurrency: row.try_get("concurrency")?,
            connect_timeout_secs: row.try_get("connect_timeout_secs")?,
            command_timeout_secs: row.try_get("command_timeout_secs")?,
            host_key_policy: row.try_get("host_key_policy")?,
            schedule_enabled: row.try_get("schedule_enabled")?,
            schedule_hour: row.try_get("schedule_hour")?,
            schedule_minute: row.try_get("schedule_minute")?,
            monitor_enabled: row.try_get("monitor_enabled")?,
            monitor_interval_secs: row.try_get("monitor_interval_secs")?,
            monitor_retention_days: row.try_get("monitor_retention_days")?,
            smtp_host: row.try_get("smtp_host")?,
            smtp_port: row.try_get("smtp_port")?,
            smtp_username: row.try_get("smtp_username")?,
            has_smtp_password: row.try_get("has_smtp_password")?,
            notify_from: row.try_get("notify_from")?,
            notify_to: row.try_get("notify_to")?,
            notify_on_failure_only: row.try_get("notify_on_failure_only")?,
        })
    }

    /// The mail configuration for run notifications, or `None` when no
    /// SMTP host is configured. Unseals the password here so no caller
    /// handles ciphertext.
    pub async fn mail_config(&self) -> Result<Option<crate::notify::MailConfig>> {
        let settings = self.settings().await?;
        let (Some(host), Some(from), Some(to)) = (
            settings.smtp_host.clone(),
            settings.notify_from.clone(),
            settings.notify_to.clone(),
        ) else {
            return Ok(None);
        };
        let username = settings.smtp_username.clone().unwrap_or_default();
        let password = match self.smtp_password().await? {
            Some(password) => password,
            None => return Ok(None), // host set but no password: incomplete
        };
        Ok(Some(crate::notify::MailConfig {
            host,
            port: settings.smtp_port.clamp(1, 65535) as u16,
            username,
            password,
            from,
            to,
            failure_only: settings.notify_on_failure_only,
        }))
    }

    pub async fn update_settings(&self, input: &SettingsInput) -> Result<()> {
        if !input.path_template.contains("{device}") {
            return Err(Error::config(
                "the path template must contain {device}, otherwise devices would overwrite \
                 each other",
            ));
        }
        if input.path_template.starts_with('/') {
            return Err(Error::config(
                "the path template must be relative to the repository root",
            ));
        }

        // `None` keeps the stored token, `Some("")` clears it, anything else
        // replaces it. Sealing happens here so no caller handles the ciphertext.
        let sealed = match input.git_token.as_deref().map(str::trim) {
            None => None,
            Some("") => Some(None),
            Some(token) => Some(Some(self.key.seal(token)?)),
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE settings SET
                 path_template = $1, committer_name = $2, committer_email = $3,
                 remote_url = $4, remote_branch = $5, remote_push = $6, git_username = $7,
                 export_mode = $8, show_sensitive = $9, concurrency = $10,
                 connect_timeout_secs = $11, command_timeout_secs = $12, host_key_policy = $13,
                 schedule_enabled = $14, schedule_hour = $15, schedule_minute = $16,
                 allow_invalid_certs = $17,
                 monitor_enabled = $18, monitor_interval_secs = $19,
                 monitor_retention_days = $20,
                 smtp_host = $21, smtp_port = $22, smtp_username = $23,
                 notify_from = $24, notify_to = $25,
                 notify_on_failure_only = $26, updated_at = now()
             WHERE id",
        )
        .bind(input.path_template.trim())
        .bind(input.committer_name.trim())
        .bind(input.committer_email.trim())
        .bind(
            input
                .remote_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty()),
        )
        .bind(input.remote_branch.trim())
        .bind(input.remote_push)
        .bind(input.git_username.trim())
        .bind(&input.export_mode)
        .bind(input.show_sensitive)
        .bind(input.concurrency)
        .bind(input.connect_timeout_secs)
        .bind(input.command_timeout_secs)
        .bind(&input.host_key_policy)
        .bind(input.schedule_enabled)
        .bind(input.schedule_hour)
        .bind(input.schedule_minute)
        .bind(input.allow_invalid_certs)
        .bind(input.monitor_enabled)
        .bind(input.monitor_interval_secs)
        .bind(input.monitor_retention_days)
        .bind(
            input
                .smtp_host
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty()),
        )
        .bind(input.smtp_port)
        .bind(
            input
                .smtp_username
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty()),
        )
        .bind(
            input
                .notify_from
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty()),
        )
        .bind(
            input
                .notify_to
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty()),
        )
        .bind(input.notify_on_failure_only)
        .execute(&mut *tx)
        .await?;

        // Same convention as the git token: None keeps, Some("") clears,
        // Some(p) seals a new password.
        let sealed_smtp = match input.smtp_password.as_deref().map(str::trim) {
            None => None,
            Some("") => Some(None),
            Some(p) => Some(Some(self.key.seal(p)?)),
        };
        if let Some(value) = sealed_smtp {
            sqlx::query("UPDATE settings SET smtp_password_sealed = $1 WHERE id")
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }

        if let Some(value) = sealed {
            sqlx::query("UPDATE settings SET git_token_sealed = $1 WHERE id")
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        info!("settings updated");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

impl Db {
    pub async fn devices(&self) -> Result<Vec<DeviceRow>> {
        let rows = sqlx::query(
            "SELECT d.*, t.slug AS tenant
               FROM devices d JOIN tenants t ON t.id = d.tenant_id
              ORDER BY t.slug, d.name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(device_row).collect()
    }

    pub async fn device(&self, id: Uuid) -> Result<DeviceRow> {
        let row = sqlx::query(
            "SELECT d.*, t.slug AS tenant
               FROM devices d JOIN tenants t ON t.id = d.tenant_id
              WHERE d.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound("device"))?;
        device_row(&row)
    }

    /// Find a device by name, case-insensitively. Names are unique per tenant,
    /// so this returns the first match across tenants — enough for the command
    /// line, which addresses devices by name.
    pub async fn find_device_by_name(&self, name: &str) -> Result<Option<DeviceRow>> {
        let row = sqlx::query(
            "SELECT d.*, t.slug AS tenant
               FROM devices d JOIN tenants t ON t.id = d.tenant_id
              WHERE lower(d.name) = lower($1)
              ORDER BY t.slug
              LIMIT 1",
        )
        .bind(name.trim())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(device_row).transpose()
    }

    pub async fn create_device(&self, input: &DeviceInput) -> Result<Uuid> {
        input.validate(true)?;
        let mut tx = self.pool.begin().await?;
        let tenant_id = upsert_tenant(&mut tx, &input.tenant).await?;
        let id = Uuid::new_v4();
        let sealed = match input.secret.as_deref().filter(|s| !s.is_empty()) {
            Some(secret) => Some(self.key.seal(secret)?),
            None => None,
        };

        sqlx::query(
            "INSERT INTO devices
                 (id, tenant_id, name, host, port, username, auth_kind, secret_sealed,
                  private_key_path, tags, enabled)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(input.name.trim())
        .bind(input.host.trim())
        .bind(i32::from(input.port))
        .bind(input.username.trim())
        .bind(&input.auth_kind)
        .bind(sealed)
        .bind(
            input
                .private_key_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty()),
        )
        .bind(&input.tags)
        .bind(input.enabled)
        .execute(&mut *tx)
        .await
        .map_err(|error| duplicate_name(error, &input.name))?;

        tx.commit().await?;
        info!(device = %input.name, "device created");
        Ok(id)
    }

    pub async fn update_device(&self, id: Uuid, input: &DeviceInput) -> Result<()> {
        input.validate(false)?;
        let existing = self.device(id).await?;

        // Changing to password auth without typing a password is only valid if
        // one is already stored.
        if input.auth_kind == "password"
            && input.secret.as_deref().unwrap_or("").is_empty()
            && !(existing.auth_kind == "password" && existing.has_secret)
        {
            return Err(Error::config("a password is required"));
        }

        let mut tx = self.pool.begin().await?;
        let tenant_id = upsert_tenant(&mut tx, &input.tenant).await?;

        sqlx::query(
            "UPDATE devices SET
                 tenant_id = $2, name = $3, host = $4, port = $5, username = $6,
                 auth_kind = $7, private_key_path = $8, tags = $9, enabled = $10,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(input.name.trim())
        .bind(input.host.trim())
        .bind(i32::from(input.port))
        .bind(input.username.trim())
        .bind(&input.auth_kind)
        .bind(
            input
                .private_key_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty()),
        )
        .bind(&input.tags)
        .bind(input.enabled)
        .execute(&mut *tx)
        .await
        .map_err(|error| duplicate_name(error, &input.name))?;

        // An empty secret field means "keep what is stored"; switching away from
        // password auth drops it, so a stale credential is not left behind.
        match input.secret.as_deref().filter(|s| !s.is_empty()) {
            Some(secret) => {
                sqlx::query("UPDATE devices SET secret_sealed = $2 WHERE id = $1")
                    .bind(id)
                    .bind(self.key.seal(secret)?)
                    .execute(&mut *tx)
                    .await?;
            }
            None if input.auth_kind == "agent" => {
                sqlx::query("UPDATE devices SET secret_sealed = NULL WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {}
        }

        tx.commit().await?;
        info!(device = %input.name, "device updated");
        Ok(())
    }

    pub async fn delete_device(&self, id: Uuid) -> Result<String> {
        let device = self.device(id).await?;
        sqlx::query("DELETE FROM devices WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        info!(device = %device.name, "device deleted");
        Ok(device.name)
    }

    pub async fn set_device_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        sqlx::query("UPDATE devices SET enabled = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record what a device told us about itself during a probe.
    pub async fn record_probe(&self, id: Uuid, info: &crate::routeros::RouterInfo) -> Result<()> {
        sqlx::query(
            "UPDATE devices SET
                 identity = COALESCE($2, identity),
                 firmware = COALESCE($3, firmware),
                 model    = COALESCE($4, model),
                 serial   = COALESCE($5, serial),
                 last_seen_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(info.identity.as_deref())
        .bind(info.version.as_deref())
        .bind(info.model.as_deref())
        .bind(info.serial.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runtime configuration
// ---------------------------------------------------------------------------

impl Db {
    /// Assemble the [`Config`] the engine runs on, decrypting credentials.
    ///
    /// `repo_path` comes from the environment rather than the database: where the
    /// backup working tree lives is a deployment decision (a mounted volume),
    /// not something to change from a browser.
    pub async fn runtime_config(&self, repo_path: PathBuf) -> Result<Config> {
        let settings = self.settings().await?;
        let token = self.git_token().await?;

        let remote = settings.remote_url.as_ref().map(|url| Remote {
            name: "origin".to_string(),
            url: url.clone(),
            branch: settings.remote_branch.clone(),
            push: settings.remote_push,
            auth: match &token {
                Some(token) => GitAuth::Token {
                    username: settings.git_username.clone(),
                    token: token.clone(),
                },
                None => GitAuth::None,
            },
            allow_invalid_certs: settings.allow_invalid_certs,
        });

        let config = Config {
            general: General {
                concurrency: settings.concurrency.max(1) as usize,
                connect_timeout_secs: settings.connect_timeout_secs.max(1) as u64,
                command_timeout_secs: settings.command_timeout_secs.max(1) as u64,
                host_key_policy: HostKeyPolicy::parse(&settings.host_key_policy),
                known_hosts: None,
            },
            backup: Backup {
                repo_path,
                path_template: settings.path_template.clone(),
                committer: Committer {
                    name: settings.committer_name.clone(),
                    email: settings.committer_email.clone(),
                },
                remote,
            },
            export: Export {
                mode: ExportMode::parse(&settings.export_mode),
                show_sensitive: settings.show_sensitive,
                normalize_header: true,
            },
            devices: self.runtime_devices().await?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Devices with their credentials decrypted, ready to connect.
    async fn runtime_devices(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query(
            "SELECT d.id, d.tenant_id, t.slug AS tenant, d.name, d.host, d.port, d.username,
                    d.auth_kind, d.secret_sealed, d.private_key_path, d.tags, d.enabled
               FROM devices d JOIN tenants t ON t.id = d.tenant_id
              ORDER BY t.slug, d.name",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut devices = Vec::with_capacity(rows.len());
        for row in &rows {
            let name: String = row.try_get("name")?;
            let kind: String = row.try_get("auth_kind")?;
            let sealed: Option<String> = row.try_get("secret_sealed")?;
            let key_path: Option<String> = row.try_get("private_key_path")?;

            // A failure here means the master key no longer matches what sealed
            // these rows, which is fleet-wide rather than specific to one
            // device — so it aborts instead of quietly skipping routers.
            let secret = match &sealed {
                Some(sealed) => Some(self.key.open(sealed)?),
                None => None,
            };

            let auth = match kind.as_str() {
                "password" => DeviceAuth::Password(secret.unwrap_or_default()),
                "key" => DeviceAuth::Key {
                    private_key: PathBuf::from(key_path.unwrap_or_default()),
                    passphrase: secret,
                },
                _ => DeviceAuth::Agent,
            };

            devices.push(Device {
                id: row.try_get("id")?,
                tenant_id: row.try_get("tenant_id")?,
                name,
                host: row.try_get("host")?,
                port: u16::try_from(row.try_get::<i32, _>("port")?).unwrap_or(22),
                username: row.try_get("username")?,
                auth,
                tenant: row.try_get("tenant")?,
                tags: row.try_get("tags")?,
                enabled: row.try_get("enabled")?,
            });
        }
        Ok(devices)
    }

    /// The decrypted GitHub token, if one is stored.
    pub async fn git_token(&self) -> Result<Option<String>> {
        let (sealed,): (Option<String>,) =
            sqlx::query_as("SELECT git_token_sealed FROM settings WHERE id")
                .fetch_one(&self.pool)
                .await?;
        match sealed {
            Some(sealed) => Ok(Some(self.key.open(&sealed)?)),
            None => Ok(None),
        }
    }

    pub async fn smtp_password(&self) -> Result<Option<String>> {
        let (sealed,): (Option<String>,) =
            sqlx::query_as("SELECT smtp_password_sealed FROM settings WHERE id")
                .fetch_one(&self.pool)
                .await?;
        match sealed {
            Some(sealed) => Ok(Some(self.key.open(&sealed)?)),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Run history
// ---------------------------------------------------------------------------

impl Db {
    /// Open a run row so the UI can follow it while it happens.
    pub async fn start_run(&self, trigger: &str, dry_run: bool) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO backup_runs (id, trigger, dry_run) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(trigger)
            .bind(dry_run)
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    /// Close out a run: totals, per-device events, device observations, log.
    pub async fn finish_run(&self, run_id: Uuid, report: &RunReport, log: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        let status = if report.exit_code() == 0 {
            "completed"
        } else {
            "failed"
        };
        let push_detail = match &report.push {
            PushReport::Pushed => None,
            PushReport::Skipped(reason) => Some((*reason).to_string()),
            PushReport::Failed(error) => Some(error.clone()),
        };

        sqlx::query(
            "UPDATE backup_runs SET
                 finished_at = now(), status = $2, changed = $3, unchanged = $4, failed = $5,
                 pushed = $6, push_detail = $7, log = $8
             WHERE id = $1",
        )
        .bind(run_id)
        .bind(status)
        .bind(i32::try_from(report.changed()).unwrap_or(i32::MAX))
        .bind(i32::try_from(report.unchanged()).unwrap_or(i32::MAX))
        .bind(i32::try_from(report.failed()).unwrap_or(i32::MAX))
        .bind(matches!(report.push, PushReport::Pushed))
        .bind(push_detail)
        .bind(log)
        .execute(&mut *tx)
        .await?;

        for device in &report.devices {
            // A single run legitimately spans tenants; the setting is
            // transaction-local and simply overwritten per row.
            Db::set_tenant(&mut tx, device.tenant_id).await?;

            let (outcome, commit_id, insertions, deletions) = match &device.outcome {
                Outcome::Unchanged => ("unchanged", None, 0, 0),
                Outcome::Committed(commit) => (
                    "committed",
                    Some(commit.id.to_string()),
                    commit.insertions as i32,
                    commit.deletions as i32,
                ),
                Outcome::WouldChange => ("would_change", None, 0, 0),
                Outcome::Failed(_) => ("failed", None, 0, 0),
            };
            let detail = device.detail();

            sqlx::query(
                "INSERT INTO backup_events
                     (id, run_id, tenant_id, device_id, outcome, commit_id, repo_path,
                      insertions, deletions, firmware, detail, elapsed_ms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(Uuid::new_v4())
            .bind(run_id)
            .bind(device.tenant_id)
            .bind(device.device_id)
            .bind(outcome)
            .bind(commit_id)
            .bind(device.path.to_string_lossy().into_owned())
            .bind(insertions)
            .bind(deletions)
            .bind(device.firmware.as_deref())
            .bind(&detail)
            .bind(i64::try_from(device.elapsed.as_millis()).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;

            // Only a reachable device updates its observed facts; a failure must
            // not blank out the last known firmware.
            let reachable = !device.outcome.is_failure();
            sqlx::query(
                "UPDATE devices SET
                     last_outcome = $2,
                     last_detail = $3,
                     last_seen_at = CASE WHEN $4 THEN now() ELSE last_seen_at END,
                     identity = CASE WHEN $4 THEN COALESCE($5, identity) ELSE identity END,
                     firmware = CASE WHEN $4 THEN COALESCE($6, firmware) ELSE firmware END,
                     model    = CASE WHEN $4 THEN COALESCE($7, model)    ELSE model    END,
                     serial   = CASE WHEN $4 THEN COALESCE($8, serial)   ELSE serial   END
                 WHERE id = $1",
            )
            .bind(device.device_id)
            .bind(outcome)
            .bind(&detail)
            .bind(reachable)
            .bind(device.identity.as_deref())
            .bind(device.firmware.as_deref())
            .bind(device.model.as_deref())
            .bind(device.serial.as_deref())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        debug!(%run_id, "recorded backup run");
        Ok(())
    }

    /// Mark a run that could not even start (bad settings, unusable repository).
    pub async fn abort_run(&self, run_id: Uuid, error: &str, log: &str) -> Result<()> {
        sqlx::query(
            "UPDATE backup_runs SET finished_at = now(), status = 'failed',
                 push_detail = $2, log = $3 WHERE id = $1",
        )
        .bind(run_id)
        .bind(error)
        .bind(log)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Has a scheduled run started since `since`? Guards the scheduler against
    /// firing twice within the same minute, and against re-firing on restart.
    pub async fn scheduled_run_since(&self, since: DateTime<Utc>) -> Result<bool> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM backup_runs WHERE trigger = 'schedule' AND started_at >= $1",
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn recent_runs(&self, limit: i64) -> Result<Vec<RunRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM backup_runs ORDER BY started_at DESC LIMIT $1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn run(&self, id: Uuid) -> Result<RunRow> {
        sqlx::query_as("SELECT * FROM backup_runs WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("run"))
    }

    pub async fn run_events(&self, run_id: Uuid) -> Result<Vec<EventRow>> {
        Ok(sqlx::query_as(
            "SELECT e.*, d.name AS device_name
               FROM backup_events e JOIN devices d ON d.id = e.device_id
              WHERE e.run_id = $1
              ORDER BY d.name",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn device_events(&self, device_id: Uuid, limit: i64) -> Result<Vec<EventRow>> {
        Ok(sqlx::query_as(
            "SELECT e.*, d.name AS device_name
               FROM backup_events e JOIN devices d ON d.id = e.device_id
              WHERE e.device_id = $1
              ORDER BY e.created_at DESC LIMIT $2",
        )
        .bind(device_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Where a device's file lives, computed from a row rather than from a
/// [`crate::config::Device`], so the UI does not need decrypted credentials to
/// show a path.
pub fn backup_path_for(device: &DeviceRow, template: &str) -> PathBuf {
    crate::config::render_backup_path(template, &device.tenant, &device.name, &device.host)
}

fn device_row(row: &sqlx::postgres::PgRow) -> Result<DeviceRow> {
    Ok(DeviceRow {
        id: row.try_get("id")?,
        tenant_id: row.try_get("tenant_id")?,
        tenant: row.try_get("tenant")?,
        name: row.try_get("name")?,
        host: row.try_get("host")?,
        port: u16::try_from(row.try_get::<i32, _>("port")?).unwrap_or(22),
        username: row.try_get("username")?,
        auth_kind: row.try_get("auth_kind")?,
        has_secret: row.try_get::<Option<String>, _>("secret_sealed")?.is_some(),
        private_key_path: row.try_get("private_key_path")?,
        tags: row.try_get("tags")?,
        enabled: row.try_get("enabled")?,
        identity: row.try_get("identity")?,
        firmware: row.try_get("firmware")?,
        model: row.try_get("model")?,
        serial: row.try_get("serial")?,
        last_seen_at: row.try_get("last_seen_at")?,
        last_outcome: row.try_get("last_outcome")?,
        last_detail: row.try_get("last_detail")?,
    })
}

/// Tenants are created on first mention, so an operator never has to set one up
/// before adding a device.
async fn upsert_tenant(tx: &mut Transaction<'_, Postgres>, slug: &str) -> Result<Uuid> {
    let slug = crate::config::slugify(slug);
    let existing: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some((id,)) = existing {
        return Ok(id);
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}

/// Seconds left in the throttle window after `last`, if it has not elapsed.
fn remaining(last: Option<DateTime<Utc>>) -> Option<i64> {
    let last = last?;
    let unlock_at = last + chrono::Duration::minutes(LOGIN_WINDOW_MINUTES);
    let seconds = (unlock_at - Utc::now()).num_seconds();
    (seconds > 0).then_some(seconds)
}

fn duplicate_name(error: sqlx::Error, name: &str) -> Error {
    match error {
        sqlx::Error::Database(ref e) if e.is_unique_violation() => Error::config(format!(
            "a device named `{name}` already exists in this tenant"
        )),
        other => other.into(),
    }
}

// ---------------------------------------------------------------------------
// Monitoring
// ---------------------------------------------------------------------------

/// Map a `device_samples` row (joined with device and tenant names) to a
/// [`crate::monitor::Sample`]. Shared by every read query in this block so
/// the column list cannot drift between them.
fn sample_of_row(row: &sqlx::postgres::PgRow) -> Result<crate::monitor::Sample> {
    Ok(crate::monitor::Sample {
        device_id: row.try_get("device_id")?,
        device: row.try_get("device")?,
        tenant: row.try_get("tenant")?,
        captured_at: row.try_get("captured_at")?,
        cpu_load: row.try_get("cpu_load")?,
        free_memory: row.try_get("free_memory")?,
        total_memory: row.try_get("total_memory")?,
        free_hdd: row.try_get("free_hdd")?,
        total_hdd: row.try_get("total_hdd")?,
        uptime_secs: row.try_get("uptime_secs")?,
        voltage: row.try_get("voltage")?,
        temperature: row.try_get("temperature")?,
        extra: row.try_get("extra")?,
    })
}

impl Db {
    /// Store one sweep of monitor samples. One statement per sample: a sweep
    /// is small (one row per enabled device per interval), and per-row inserts
    /// keep a single bad row from costing the whole batch.
    pub async fn insert_samples(&self, samples: &[crate::monitor::Sample]) -> Result<()> {
        for sample in samples {
            sqlx::query(
                "INSERT INTO device_samples
                     (id, device_id, captured_at, cpu_load, free_memory, total_memory,
                      free_hdd, total_hdd, uptime_secs, voltage, temperature, extra)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(sample.device_id)
            .bind(sample.captured_at)
            .bind(sample.cpu_load)
            .bind(sample.free_memory)
            .bind(sample.total_memory)
            .bind(sample.free_hdd)
            .bind(sample.total_hdd)
            .bind(sample.uptime_secs)
            .bind(sample.voltage)
            .bind(sample.temperature)
            .bind(&sample.extra)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Latest sample per device, for the dashboard.
    pub async fn latest_samples(&self) -> Result<Vec<crate::monitor::Sample>> {
        let rows = sqlx::query(
            "SELECT s.device_id, s.captured_at, s.cpu_load, s.free_memory, s.total_memory,
                    s.free_hdd, s.total_hdd, s.uptime_secs, s.voltage, s.temperature,
                    s.extra, d.name AS device, t.slug AS tenant
               FROM device_samples s
               JOIN devices d ON d.id = s.device_id
               JOIN tenants t ON t.id = d.tenant_id
              WHERE s.captured_at = (
                    SELECT max(captured_at) FROM device_samples
                     WHERE device_id = s.device_id)
              ORDER BY t.slug, d.name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sample_of_row).collect()
    }

    /// The most recent `limit` samples for one device, oldest first — the
    /// natural order for a time-series chart.
    pub async fn device_samples(
        &self,
        device_id: Uuid,
        limit: i64,
    ) -> Result<Vec<crate::monitor::Sample>> {
        let rows = sqlx::query(
            "SELECT s.device_id, s.captured_at, s.cpu_load, s.free_memory, s.total_memory,
                    s.free_hdd, s.total_hdd, s.uptime_secs, s.voltage, s.temperature, s.extra,
                    d.name AS device, t.slug AS tenant
               FROM (
                    SELECT * FROM device_samples WHERE device_id = $1
                    ORDER BY captured_at DESC LIMIT $2
               ) s
               JOIN devices d ON d.id = s.device_id
               JOIN tenants t ON t.id = d.tenant_id
              ORDER BY s.captured_at ASC",
        )
        .bind(device_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sample_of_row).collect()
    }

    /// Drop samples older than the retention window. Called from the monitor
    /// loop; safe to run as often as every tick.
    pub async fn prune_samples(&self, retention_days: i32) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM device_samples WHERE captured_at < now() - ($1 || ' days')::interval",
        )
        .bind(retention_days.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// Backup archive: dump and restore
// ---------------------------------------------------------------------------

impl Db {
    /// A full logical dump as one SQL script.
    ///
    /// Rendering is delegated to the database itself: each table is read as
    /// `to_jsonb` rows and replayed with `jsonb_populate_recordset` against the
    /// *current* table type. That keeps column lists out of this file — a
    /// migration that adds a column cannot break the backup — and sidesteps
    /// every literal-quoting edge case, because Postgres renders and re-parses
    /// its own values.
    pub async fn dump_sql(&self) -> Result<String> {
        use sqlx::Row;

        let mut out = String::new();
        out.push_str("-- DonDude logical dump\n");
        out.push_str(&format!(
            "-- written by {} at {}\n\n",
            crate::VERSION,
            chrono::Utc::now().to_rfc3339()
        ));
        // No BEGIN/COMMIT here: restore_sql wraps the replay in its own
        // transaction, and a nested BEGIN through raw_sql would warn.

        // Wipe in reverse dependency order; CASCADE covers the rest anyway.
        let tables = crate::backup_archive::tables();
        out.push_str("\nTRUNCATE ");
        out.push_str(
            &tables
                .iter()
                .rev()
                .map(|t| crate::backup_archive::quote_ident(t))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str(" CASCADE;\n");

        for table in tables {
            let ident = crate::backup_archive::quote_ident(table);
            // Table names come from the const TABLES list, not user input.
            let rows = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "SELECT to_jsonb(t) AS row FROM {ident} t"
            )))
            .fetch_all(&self.pool)
            .await?;
            let mut json_rows: Vec<String> = Vec::with_capacity(rows.len());
            for row in &rows {
                let json: serde_json::Value = row.try_get("row")?;
                let text = serde_json::to_string(&json)
                    .map_err(|e| Error::config(format!("dump: {e}")))?;
                json_rows.push(text);
            }
            out.push_str(&format!("\n-- {} rows in {table}\n", json_rows.len()));
            if !json_rows.is_empty() {
                out.push_str(&format!(
                    "INSERT INTO {ident} SELECT * FROM jsonb_populate_recordset(null::{table}, '[{}] '::jsonb);\n",
                    json_rows.join(", ")
                ));
            }
        }

        Ok(out)
    }

    /// Replay a dump produced by [`Db::dump_sql`] inside one transaction.
    pub async fn restore_sql(&self, sql: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // The dump being replayed was produced by dump_sql and sealed with
        // the master key; an attacker who could tamper with it could tamper
        // with the database directly.
        for statement in crate::backup_archive::split_statements(sql) {
            sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
