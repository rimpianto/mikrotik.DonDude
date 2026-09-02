//! `dondude` — command line entry point.
//!
//! Two faces on one binary:
//!
//! * `dondude serve` runs the web interface. This is what the container starts.
//! * the other subcommands are for operators and cron: they read the same
//!   database the UI writes, so there is one source of truth and no config file
//!   to keep in sync.
//!
//! Deployment settings come from the environment, because that is what a
//! container passes in. Everything an operator changes day to day lives in the
//! database and is edited in the browser.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, ArgGroup, Args, Parser, Subcommand};

use mikrotik_dondude::backup::{self, DeviceReport, ProgressSink, PushReport, RunOptions};
use mikrotik_dondude::config::DeviceFilter;
use mikrotik_dondude::crypto::{MASTER_KEY_ENV, MasterKey};
use mikrotik_dondude::db::Db;
use mikrotik_dondude::web::{self, AppState};
use mikrotik_dondude::{Config, init_tracing};

/// Where the backup working tree lives. A mounted volume in Docker.
const ENV_REPO_PATH: &str = "DONDUDE_REPO_PATH";
const DEFAULT_REPO_PATH: &str = "/data/backups";
/// Address the web interface binds to.
const ENV_BIND: &str = "DONDUDE_BIND";
const DEFAULT_BIND: &str = "0.0.0.0:8080";
const ENV_DATABASE_URL: &str = "DATABASE_URL";
const ENV_POOL: &str = "DONDUDE_DB_POOL";

#[derive(Debug, Parser)]
#[command(
    name = "dondude",
    version,
    about = "Multi-tenant MikroTik RouterOS fleet manager",
    long_about = "DonDude captures RouterOS `/export` configurations across a device fleet and \
                  versions them in a dedicated Git repository. Run `dondude serve` for the web \
                  interface.",
    propagate_version = true
)]
struct Cli {
    /// Increase log verbosity (repeatable). `RUST_LOG` overrides this.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Only log warnings and errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the web interface.
    Serve(ServeArgs),

    /// Capture and version device configurations.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Backup {
        #[command(subcommand)]
        action: BackupCommand,
    },

    /// Poll device state once and print the samples (no web server needed).
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Monitor {
        #[command(subcommand)]
        action: MonitorCommand,
    },

    /// Interact with a single device.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Device {
        #[command(subcommand)]
        action: DeviceCommand,
    },

    /// Inspect the device inventory.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Fleet {
        #[command(subcommand)]
        action: FleetCommand,
    },

    /// Fleet-wide settings, including the backup remote.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Settings {
        #[command(subcommand)]
        action: SettingsCommand,
    },

    /// Operator accounts.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    User {
        #[command(subcommand)]
        action: UserCommand,
    },

    /// Database schema and connectivity.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Db {
        #[command(subcommand)]
        action: DbCommand,
    },

    /// Generate a master key for encrypting stored credentials.
    Keygen,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address to listen on. Defaults to $DONDUDE_BIND, then 0.0.0.0:8080.
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,

    /// Start without applying pending migrations.
    #[arg(long)]
    skip_migrations: bool,
}

#[derive(Debug, Subcommand)]
enum MonitorCommand {
    /// Sample every enabled device once and report.
    Poll,
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    /// Back up every enabled device, then push.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Only this device. Repeatable. Includes it even if disabled.
    #[arg(long = "device", short = 'd', value_name = "NAME")]
    devices: Vec<String>,

    /// Only devices carrying this tag. Repeatable.
    #[arg(long = "tag", short = 't', value_name = "TAG")]
    tags: Vec<String>,

    /// Only devices in this tenant. Repeatable.
    #[arg(long = "tenant", value_name = "TENANT")]
    tenants: Vec<String>,

    /// Include devices that are switched off.
    #[arg(long)]
    include_disabled: bool,

    /// Capture and compare, but write, commit and push nothing.
    #[arg(long)]
    dry_run: bool,

    /// Commit locally without pushing.
    #[arg(long)]
    no_push: bool,

    /// Override the configured parallelism.
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// Connect and report identity and firmware, without exporting.
    Test { name: String },
}

#[derive(Debug, Subcommand)]
enum FleetCommand {
    /// List configured devices.
    List,

    /// Add a device, or update it with --update.
    ///
    /// Everything the web form asks for, so a fleet can be provisioned from a
    /// script instead of by hand.
    Add(AddArgs),

    /// Delete a device. Its Git history is kept.
    Remove { name: String },

    /// Include a device in fleet-wide runs.
    Enable { name: String },

    /// Exclude a device from fleet-wide runs.
    Disable { name: String },
}

/// Exactly one credential source is required when creating a device.
#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("credentials")
        .args(["password", "password_env", "key", "agent"])
))]
struct AddArgs {
    /// Unique name. Becomes the file name in the backup repository.
    #[arg(long)]
    name: String,

    /// Hostname or IP address.
    #[arg(long)]
    host: String,

    #[arg(long, default_value_t = 22)]
    port: u16,

    /// RouterOS user to log in as.
    #[arg(long = "user", value_name = "USER")]
    username: String,

    /// Grouping; becomes a folder in the repository.
    #[arg(long, default_value = "default")]
    tenant: String,

    /// Tag for filtering runs. Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,

    /// SSH password. Ends up in your shell history — prefer --password-env.
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,

    /// Environment variable holding the SSH password.
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,

    /// SSH private key path, as seen from inside the container.
    #[arg(long, value_name = "FILE")]
    key: Option<String>,

    /// Environment variable holding the key passphrase.
    #[arg(long, value_name = "VAR", requires = "key")]
    key_passphrase_env: Option<String>,

    /// Authenticate through a running ssh-agent.
    #[arg(long)]
    agent: bool,

    /// Add the device but exclude it from fleet-wide runs.
    #[arg(long)]
    disabled: bool,

    /// Update the device if it already exists, instead of failing.
    ///
    /// Makes a provisioning script safe to re-run. Credentials are left alone
    /// unless a new one is given.
    #[arg(long)]
    update: bool,
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Show the current settings.
    Show,

    /// Configure the backup remote.
    Remote(RemoteArgs),

    /// Check the stored remote: connect and list its branches.
    Test,
}

#[derive(Debug, Args)]
struct RemoteArgs {
    /// Repository URL. Pass an empty string to keep backups local only.
    #[arg(long)]
    url: Option<String>,

    #[arg(long)]
    branch: Option<String>,

    /// Username sent with the token. GitHub ignores it.
    #[arg(long)]
    username: Option<String>,

    /// Access token. Ends up in your shell history — prefer --token-env.
    #[arg(long, value_name = "TOKEN", conflicts_with_all = ["token_env", "clear_token"])]
    token: Option<String>,

    /// Environment variable holding the access token.
    #[arg(long, value_name = "VAR", conflicts_with = "clear_token")]
    token_env: Option<String>,

    /// Forget the stored token.
    #[arg(long)]
    clear_token: bool,

    /// Push after each run.
    #[arg(long, overrides_with = "no_push")]
    push: bool,

    /// Commit locally without pushing.
    #[arg(long)]
    no_push: bool,

    /// Accept an untrusted TLS certificate, for a self-hosted instance with a
    /// self-signed one. Disables verification for the push.
    #[arg(long)]
    insecure_tls: bool,

    /// Require a valid TLS certificate again.
    #[arg(long, conflicts_with = "insecure_tls")]
    secure_tls: bool,

    /// Connect to the remote afterwards to check it.
    #[arg(long)]
    test: bool,
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    /// Create an operator account.
    Add {
        username: String,
        /// Read from stdin when omitted.
        #[arg(long)]
        password: Option<String>,
    },
    /// Change an operator's password.
    Passwd {
        username: String,
        #[arg(long)]
        password: Option<String>,
    },
    /// List operator accounts.
    List,
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Apply pending migrations.
    Migrate,
    /// Verify connectivity and report the server version.
    Check,
    /// Write an encrypted, self-contained backup (database + .env + known_hosts).
    Backup {
        /// Directory to write to (default: current directory).
        #[arg(value_name = "DIR", default_value = ".")]
        path: PathBuf,
    },
    /// Restore a backup written by `db backup`. REPLACES the current data.
    Restore {
        /// The .dud archive to restore.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Also write the restored .env next to the current one (as .env.restored).
        #[arg(long)]
        write_env: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.quiet);

    match dispatch(cli).await {
        Ok(code) => code,
        Err(error) => {
            // The chain matters: "connection refused" alone never says to what.
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        // Needs neither a database nor a key: it is what you run first.
        Command::Keygen => {
            println!("{}", MasterKey::generate()?);
            eprintln!(
                "\nStore this as {MASTER_KEY_ENV}. It decrypts every credential DonDude keeps;\n\
                 without it the stored router passwords and GitHub token are unreadable."
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve(args) => serve(args).await,
        Command::Db { action } => db_command(action).await,
        Command::User { action } => user_command(action).await,
        Command::Fleet { action } => fleet(action).await,
        Command::Settings { action } => settings(action).await,
        Command::Device {
            action: DeviceCommand::Test { name },
        } => device_test(&name).await,
        Command::Monitor {
            action: MonitorCommand::Poll,
        } => monitor_poll().await,
        Command::Backup {
            action: BackupCommand::Run(args),
        } => backup_run(args).await,
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

fn repo_path() -> PathBuf {
    PathBuf::from(std::env::var(ENV_REPO_PATH).unwrap_or_else(|_| DEFAULT_REPO_PATH.to_string()))
}

/// Connect to PostgreSQL with the master key loaded.
///
/// Both are required and both fail loudly: a missing key must never degrade into
/// storing credentials in the clear, and a missing DSN has no sensible default.
async fn connect() -> Result<Arc<Db>> {
    let dsn = std::env::var(ENV_DATABASE_URL).map_err(|_| {
        anyhow::anyhow!(
            "{ENV_DATABASE_URL} is not set (for example \
             postgres://dondude:secret@db:5432/dondude)"
        )
    })?;
    let pool_size = std::env::var(ENV_POOL)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    let key = MasterKey::from_env()?;
    Ok(Arc::new(
        Db::connect(&dsn, pool_size, key)
            .await
            .context("connecting to PostgreSQL")?,
    ))
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

async fn serve(args: ServeArgs) -> Result<ExitCode> {
    let db = connect().await?;

    // Migrating on start-up is deliberate: a container should come up ready,
    // without a separate one-shot step in the compose file.
    if !args.skip_migrations {
        db.migrate().await?;
    }
    db.recover_after_restart().await?;

    let repo_path = repo_path();
    std::fs::create_dir_all(&repo_path)
        .with_context(|| format!("creating {}", repo_path.display()))?;

    if db.user_count().await? == 0 {
        eprintln!("No operator account yet — open the web interface to create one.");
    }

    let bind = args
        .bind
        .or_else(|| std::env::var(ENV_BIND).ok())
        .unwrap_or_else(|| DEFAULT_BIND.to_string());

    let state = AppState::new(db, repo_path);
    web::spawn_monitor(state.clone());
    crate::web::spawn_scheduler(state.clone());
    web::serve(&bind, state).await?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// backup / device / fleet
// ---------------------------------------------------------------------------

/// Prints each device's outcome as it lands, so a long run is not silent.
struct CliProgress;

impl ProgressSink for CliProgress {
    fn info(&self, message: &str) {
        println!("  {message}");
    }

    fn device(&self, report: &DeviceReport) {
        println!(
            "  {:<24} {:<12} {}",
            report.device,
            report.outcome.label(),
            report.detail()
        );
    }
}

async fn backup_run(args: RunArgs) -> Result<ExitCode> {
    let db = connect().await?;
    let config = db.runtime_config(repo_path()).await?;

    let options = RunOptions {
        filter: DeviceFilter {
            names: args.devices,
            tags: args.tags,
            tenants: args.tenants,
            include_disabled: args.include_disabled,
        },
        dry_run: args.dry_run,
        no_push: args.no_push,
        concurrency: args.concurrency,
    };

    // The same gate the web interface uses, so a cron job and a click in the
    // browser cannot interleave commits in the backup repository. Held until
    // the end of this function; dropping it releases the lock.
    let _run_lock = db.try_lock_run().await?.ok_or_else(|| {
        anyhow::anyhow!(
            "a backup run is already in progress (started from the web interface or another \
             command); wait for it to finish"
        )
    })?;

    let run_id = db
        .start_run(mikrotik_dondude::web::runner::TRIGGER_CLI, args.dry_run)
        .await?;
    let report = match backup::run(&config, &options, &CliProgress).await {
        Ok(report) => report,
        Err(error) => {
            let message = mikrotik_dondude::error::chain(&error);
            db.abort_run(run_id, &message, "").await.ok();
            return Err(error.into());
        }
    };
    db.finish_run(run_id, &report, "").await?;

    println!();
    print_report(&report);

    Ok(if report.exit_code() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn print_report(report: &backup::RunReport) {
    if report.devices.is_empty() {
        println!("No devices matched.");
        return;
    }

    let width = |f: fn(&DeviceReport) -> usize| report.devices.iter().map(f).max().unwrap_or(0);
    let name_w = width(|d| d.device.len()).max(6);
    let tenant_w = width(|d| d.tenant.len()).max(6);
    let fw_w = width(|d| d.firmware.as_deref().unwrap_or("-").len()).max(8);

    println!(
        "{:<name_w$}  {:<tenant_w$}  {:<fw_w$}  {:<12}  DETAIL",
        "DEVICE", "TENANT", "FIRMWARE", "OUTCOME"
    );
    for device in &report.devices {
        println!(
            "{:<name_w$}  {:<tenant_w$}  {:<fw_w$}  {:<12}  {}",
            device.device,
            device.tenant,
            device.firmware.as_deref().unwrap_or("-"),
            device.outcome.label(),
            device.detail()
        );
    }

    println!("\n{}", report.summary());
    match &report.push {
        PushReport::Pushed => println!("Pushed to the backup remote."),
        PushReport::Skipped(reason) => println!("Push skipped: {reason}."),
        PushReport::Failed(error) => println!("Push FAILED: {error}"),
    }
}

async fn device_test(name: &str) -> Result<ExitCode> {
    let db = connect().await?;
    let config: Config = db.runtime_config(repo_path()).await?;
    let device = config
        .find_device(name)
        .ok_or_else(|| anyhow::anyhow!("no device named `{name}`"))?;
    let id = device.id;

    let info = backup::test_device(&config, name).await?;
    db.record_probe(id, &info).await?;

    println!("{name}: reachable, {}", info.describe());
    for (label, value) in [
        ("identity", &info.identity),
        ("routeros", &info.version),
        ("model", &info.model),
        ("serial", &info.serial),
        ("architecture", &info.architecture),
    ] {
        if let Some(value) = value {
            println!("  {label:<13}{value}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn fleet(action: FleetCommand) -> Result<ExitCode> {
    match action {
        FleetCommand::List => fleet_list().await,
        FleetCommand::Add(args) => fleet_add(args).await,
        FleetCommand::Remove { name } => {
            let db = connect().await?;
            let device = db
                .find_device_by_name(&name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no device named `{name}`"))?;
            db.delete_device(device.id).await?;
            println!("Removed `{}`. Its history in Git is kept.", device.name);
            Ok(ExitCode::SUCCESS)
        }
        FleetCommand::Enable { name } => set_enabled(&name, true).await,
        FleetCommand::Disable { name } => set_enabled(&name, false).await,
    }
}

async fn set_enabled(name: &str, enabled: bool) -> Result<ExitCode> {
    let db = connect().await?;
    let device = db
        .find_device_by_name(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no device named `{name}`"))?;
    db.set_device_enabled(device.id, enabled).await?;
    println!(
        "`{}` is now {}.",
        device.name,
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(ExitCode::SUCCESS)
}

/// Add or update one device.
///
/// Built for provisioning scripts: with `--update` it is idempotent, and an
/// omitted credential leaves the stored one alone.
async fn fleet_add(args: AddArgs) -> Result<ExitCode> {
    let db = connect().await?;

    let (auth_kind, secret, private_key_path) = device_credentials(&args)?;
    let existing = db.find_device_by_name(&args.name).await?;

    // Without an explicit credential, fall back to whatever the device already
    // uses, so `--update` can change a hostname without restating a password.
    let auth_kind = match (&auth_kind, &existing) {
        (None, Some(device)) => device.auth_kind.clone(),
        (None, None) => {
            bail!("no credential given: pass --password-env, --password, --key or --agent")
        }
        (Some(kind), _) => kind.clone(),
    };

    let input = mikrotik_dondude::db::DeviceInput {
        name: args.name.clone(),
        host: args.host,
        port: args.port,
        username: args.username,
        tenant: args.tenant,
        tags: args.tags,
        enabled: !args.disabled,
        auth_kind,
        secret,
        private_key_path,
    };

    match existing {
        Some(device) if args.update => {
            db.update_device(device.id, &input).await?;
            println!("Updated `{}`.", args.name);
        }
        Some(_) => bail!(
            "a device named `{}` already exists; pass --update to change it",
            args.name
        ),
        None => {
            db.create_device(&input).await?;
            println!("Added `{}`.", args.name);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the credential flags into what the database layer expects.
///
/// `None` for the kind means the caller gave no credential at all, which is
/// only acceptable when updating an existing device.
fn device_credentials(args: &AddArgs) -> Result<(Option<String>, Option<String>, Option<String>)> {
    if let Some(var) = &args.password_env {
        let password = std::env::var(var)
            .map_err(|_| anyhow::anyhow!("{var} is not set in the environment"))?;
        return Ok((Some("password".into()), Some(password), None));
    }
    if let Some(password) = &args.password {
        // Worth saying out loud: this line is now in the shell history and in
        // the process list of anything watching.
        eprintln!(
            "warning: --password puts the password in your shell history;              --password-env avoids that"
        );
        return Ok((Some("password".into()), Some(password.clone()), None));
    }
    if let Some(key) = &args.key {
        let passphrase = match &args.key_passphrase_env {
            Some(var) => Some(
                std::env::var(var)
                    .map_err(|_| anyhow::anyhow!("{var} is not set in the environment"))?,
            ),
            None => None,
        };
        return Ok((Some("key".into()), passphrase, Some(key.clone())));
    }
    if args.agent {
        return Ok((Some("agent".into()), None, None));
    }
    Ok((None, None, None))
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

async fn settings(action: SettingsCommand) -> Result<ExitCode> {
    let db = connect().await?;
    match action {
        SettingsCommand::Show => {
            let settings = db.settings().await?;
            println!("repository path   {}", repo_path().display());
            println!(
                "remote url        {}",
                settings
                    .remote_url
                    .as_deref()
                    .unwrap_or("(none — local only)")
            );
            println!("remote branch     {}", settings.remote_branch);
            println!("push after run    {}", settings.remote_push);
            println!("git username      {}", settings.git_username);
            println!(
                "access token      {}",
                if settings.has_git_token {
                    "stored"
                } else {
                    "(none)"
                }
            );
            println!("file layout       {}", settings.path_template);
            println!("export mode       {}", settings.export_mode);
            println!("show sensitive    {}", settings.show_sensitive);
            println!("host key policy   {}", settings.host_key_policy);
            println!(
                "tls verification  {}",
                if settings.allow_invalid_certs {
                    "DISABLED (untrusted certificates accepted)"
                } else {
                    "enforced"
                }
            );
            println!(
                "daily schedule    {}",
                if settings.schedule_enabled {
                    format!(
                        "{:02}:{:02} UTC",
                        settings.schedule_hour, settings.schedule_minute
                    )
                } else {
                    "off".to_string()
                }
            );
            println!("parallel devices  {}", settings.concurrency);
        }

        SettingsCommand::Remote(args) => {
            let current = db.settings().await?;
            let mut input = current.to_input();

            if let Some(url) = &args.url {
                input.remote_url = Some(url.clone()).filter(|u| !u.trim().is_empty());
            }
            if let Some(branch) = &args.branch {
                input.remote_branch = branch.clone();
            }
            if let Some(username) = &args.username {
                input.git_username = username.clone();
            }
            if args.push {
                input.remote_push = true;
            }
            if args.no_push {
                input.remote_push = false;
            }
            if args.insecure_tls {
                input.allow_invalid_certs = true;
            }
            if args.secure_tls {
                input.allow_invalid_certs = false;
            }

            // `None` keeps the stored token; an empty string clears it.
            input.git_token = if args.clear_token {
                Some(String::new())
            } else if let Some(var) = &args.token_env {
                Some(
                    std::env::var(var)
                        .map_err(|_| anyhow::anyhow!("{var} is not set in the environment"))?,
                )
            } else if let Some(token) = &args.token {
                eprintln!(
                    "warning: --token puts the token in your shell history;                      --token-env avoids that"
                );
                Some(token.clone())
            } else {
                None
            };

            db.update_settings(&input).await?;
            println!("Settings saved.");

            if args.test {
                match probe_stored_remote(&db).await {
                    Ok(message) => println!("{message}"),
                    Err(error) => bail!("{error:#}"),
                }
            }
        }

        SettingsCommand::Test => match probe_stored_remote(&db).await {
            Ok(message) => println!("{message}"),
            Err(error) => bail!("{error:#}"),
        },
    }
    Ok(ExitCode::SUCCESS)
}

/// Connect to the configured remote and report what is there.
async fn probe_stored_remote(db: &Db) -> Result<String> {
    let settings = db.settings().await?;
    let url = settings
        .remote_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no repository URL configured"))?;
    let token = db.git_token().await?;
    let auth = match token {
        Some(token) => mikrotik_dondude::config::GitAuth::Token {
            username: settings.git_username.clone(),
            token,
        },
        None => mikrotik_dondude::config::GitAuth::None,
    };
    // libgit2 blocks, so keep it off the async worker.
    let branch = settings.remote_branch.clone();
    let insecure = settings.allow_invalid_certs;
    Ok(tokio::task::spawn_blocking(move || {
        mikrotik_dondude::git::probe_remote(&url, &branch, &auth, insecure)
    })
    .await
    .map_err(|_| anyhow::anyhow!("the repository worker panicked"))??)
}

async fn fleet_list() -> Result<ExitCode> {
    let db = connect().await?;
    let devices = db.devices().await?;
    if devices.is_empty() {
        println!("No devices configured. Add one in the web interface.");
        return Ok(ExitCode::SUCCESS);
    }

    let name_w = devices
        .iter()
        .map(|device| device.name.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<name_w$}  {:<22}  {:<10}  {:<8}  {:<12}  TAGS",
        "DEVICE", "ADDRESS", "TENANT", "STATE", "LAST RESULT"
    );
    for device in &devices {
        println!(
            "{:<name_w$}  {:<22}  {:<10}  {:<8}  {:<12}  {}",
            device.name,
            device.addr(),
            device.tenant,
            if device.enabled {
                "enabled"
            } else {
                "disabled"
            },
            device.last_outcome.as_deref().unwrap_or("-"),
            device.tags.join(",")
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// users / db
// ---------------------------------------------------------------------------

async fn user_command(action: UserCommand) -> Result<ExitCode> {
    let db = connect().await?;
    match action {
        UserCommand::Add { username, password } => {
            let password = resolve_password(password)?;
            db.create_user(&username, &password).await?;
            println!("Created operator `{username}`.");
        }
        UserCommand::Passwd { username, password } => {
            let password = resolve_password(password)?;
            let user = db
                .find_user(&username)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no operator named `{username}`"))?;
            db.set_password(user.id, &password).await?;
            println!("Password changed for `{username}`.");
        }
        UserCommand::List => {
            let users = db.users().await?;
            if users.is_empty() {
                println!("No operator accounts yet.");
            }
            for user in users {
                println!(
                    "{:<20}  created {}  last login {}",
                    user.username,
                    user.created_at.format("%Y-%m-%d"),
                    user.last_login_at
                        .map(|when| when.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string())
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Take the password from the flag, or read one line from stdin.
///
/// Stdin is not hidden — this is a scripting and recovery path. The normal way
/// to create the first account is the browser.
fn resolve_password(password: Option<String>) -> Result<String> {
    if let Some(password) = password {
        return Ok(password);
    }
    eprint!("Password (will be visible): ");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading the password from stdin")?;
    let password = line.trim_end_matches(['\n', '\r']).to_string();
    if password.is_empty() {
        bail!("no password given");
    }
    Ok(password)
}

async fn db_command(action: DbCommand) -> Result<ExitCode> {
    let db = connect().await?;
    match action {
        DbCommand::Migrate => {
            db.migrate().await?;
            println!("Schema is up to date.");
        }
        DbCommand::Check => {
            println!("Connected: {}", db.server_version().await?);
            println!("Devices:   {}", db.devices().await?.len());
            println!("Operators: {}", db.user_count().await?);
        }
        DbCommand::Backup { path } => {
            let code = db_backup(&db, &path).await?;
            return Ok(code);
        }
        DbCommand::Restore {
            file,
            yes,
            write_env,
        } => {
            let code = db_restore(&db, &file, yes, write_env).await?;
            return Ok(code);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One-shot monitoring sweep, for cron or a first look at the fleet.
async fn monitor_poll() -> Result<ExitCode> {
    let db = connect().await?;
    let config = db.runtime_config(repo_path()).await?;
    let report = mikrotik_dondude::monitor::poll_fleet(&db, &config).await;

    for sample in &report.samples {
        let mem = match (sample.free_memory, sample.total_memory) {
            (Some(free), Some(total)) if total > 0 => {
                format!("mem {}%", 100 - (free * 100 / total))
            }
            _ => "-".to_string(),
        };
        let uptime = sample
            .uptime_secs
            .map(|s| format!("up {}d {}h", s / 86400, (s % 86400) / 3600))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<24} cpu {:>3}%  {:<10}  {}",
            sample.device,
            sample.cpu_load.unwrap_or(0),
            mem,
            uptime
        );
    }
    for failure in &report.failures {
        println!("{:<24} FAILED: {}", failure.device, failure.error);
    }
    println!("{}", report.describe());
    Ok(ExitCode::SUCCESS)
}

/// Write an encrypted, self-contained deployment backup.
///
/// The archive is sealed with DONDUDE_MASTER_KEY: the same secret that
/// decrypts the stored credentials decrypts the backup, so there is no
/// second key to lose. Without the key the backup is unreadable — keep a
/// copy of it somewhere safe, as the manual says.
async fn db_backup(db: &Db, dir: &Path) -> Result<ExitCode> {
    let key = db.key().clone();

    let sql = db.dump_sql().await?;

    // The .env file, wherever it is. Look in the obvious places: next to the
    // working directory, then the directory the binary was started from.
    let env_file = mikrotik_dondude::backup_archive::read_env_file();
    let known_hosts = mikrotik_dondude::backup_archive::read_known_hosts();

    let input = mikrotik_dondude::backup_archive::BackupInput {
        database_sql: sql,
        env_file: env_file.clone(),
        known_hosts: known_hosts.clone(),
    };

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("dondude-backup-{timestamp}.dud"));
    input.write_archive(&path, &key)?;

    println!("Backup written: {}", path.display());
    println!();
    println!("  contains: database");
    if env_file.is_some() {
        println!("            .env");
    } else {
        println!("            .env NOT FOUND — copy it manually (it holds the master key)");
    }
    if known_hosts.is_some() {
        println!("            known_hosts");
    }
    println!();
    println!("  It is encrypted with DONDUDE_MASTER_KEY.");
    println!("  Restore with: dondude db restore {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Restore a deployment from an archive. Destructive: asks first.
async fn db_restore(db: &Db, file: &Path, yes: bool, write_env: bool) -> Result<ExitCode> {
    let key = db.key().clone();
    let archive = mikrotik_dondude::backup_archive::Archive::read(file, &key)?;

    println!(
        "Archive created {} by DonDude {}",
        archive.manifest.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
        archive.manifest.version
    );
    println!("Files: {}", archive.manifest.files.join(", "));

    let Some(sql) = archive.file("database.sql") else {
        bail!("the archive contains no database dump");
    };
    let sql = String::from_utf8_lossy(sql);

    if !yes {
        println!();
        println!("This REPLACES everything currently in the database.");
        print!("Type 'yes' to continue: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("Aborted.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    db.restore_sql(&sql).await?;
    println!("Database restored.");

    if write_env {
        if let Some(env) = archive.file(".env") {
            let target = PathBuf::from(".env.restored");
            std::fs::write(&target, env)?;
            println!(
                ".env written to {} — review and replace your .env.",
                target.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}
