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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};

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
        Command::Fleet {
            action: FleetCommand::List,
        } => fleet_list().await,
        Command::Device {
            action: DeviceCommand::Test { name },
        } => device_test(&name).await,
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
    web::spawn_scheduler(state.clone());
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
    }
    Ok(ExitCode::SUCCESS)
}
