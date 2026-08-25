//! Backup pipeline: walk the fleet, capture, version, push.
//!
//! ## Shape of a run
//!
//! ```text
//!            ┌─ capture ─┐
//! devices ───┼─ capture ─┼──▶ store + commit (serial) ──▶ push (once)
//!            └─ capture ─┘
//!              concurrent
//! ```
//!
//! Captures run concurrently up to `general.concurrency`; commits do not.
//! libgit2's index is a single file with a lock, and one commit per device only
//! makes sense if the commits are ordered, so results are folded into the
//! repository as they arrive rather than in a second batched pass. A slow device
//! therefore delays only its own commit, not the others'.
//!
//! ## Failure policy
//!
//! A device that cannot be reached is recorded in the report and the run
//! continues — one dead router must not cost the rest of the fleet its backup.
//! Only a broken configuration or an unusable backup repository aborts.
//!
//! Note that [`run`] holds a non-`Send` [`BackupRepo`] across await points, so
//! it must be driven by `block_on` (as `#[tokio::main]` does) rather than
//! handed to `tokio::spawn`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::config::{Config, Device, DeviceFilter};
use crate::error::{Error, Result};
use crate::git::{BackupRepo, Commit, CommitMeta, Stored, Synced};
use crate::routeros::{self, Capture};

/// A live view of a run in progress.
///
/// The web UI needs to show what is happening while a fleet walk is under way,
/// and the CLI does not. Rather than have the pipeline know about either, it
/// reports through this trait; `()` implements it as a no-op.
pub trait ProgressSink: Send + Sync {
    /// A free-form line for the run log.
    fn info(&self, message: &str);
    /// One device finished, for whatever reason.
    fn device(&self, report: &DeviceReport);
}

impl ProgressSink for () {
    fn info(&self, _message: &str) {}
    fn device(&self, _report: &DeviceReport) {}
}

/// Knobs for one `backup run`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub filter: DeviceFilter,
    /// Capture and compare, but write nothing and commit nothing.
    pub dry_run: bool,
    /// Force-disable the push even if the config enables it.
    pub no_push: bool,
    /// Override `general.concurrency`.
    pub concurrency: Option<usize>,
}

/// What happened to one device.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Config is identical to what is already committed.
    Unchanged,
    Committed(Commit),
    /// `--dry-run` only: this device would have been committed.
    WouldChange,
    /// Capture failed; the device keeps its previous backup.
    Failed(String),
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Committed(_) => "committed",
            Self::WouldChange => "would change",
            Self::Failed(_) => "failed",
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    pub fn is_change(&self) -> bool {
        matches!(self, Self::Committed(_) | Self::WouldChange)
    }
}

/// Per-device row of the run report.
#[derive(Debug, Clone)]
pub struct DeviceReport {
    pub device: String,
    pub device_id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub host: String,
    pub tenant: String,
    pub path: PathBuf,
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub identity: Option<String>,
    pub serial: Option<String>,
    pub outcome: Outcome,
    pub elapsed: Duration,
}

impl DeviceReport {
    /// Human-readable detail for the UI and the log.
    pub fn detail(&self) -> String {
        match &self.outcome {
            Outcome::Committed(commit) => format!("{} {}", commit.stats(), self.path.display()),
            Outcome::WouldChange | Outcome::Unchanged => self.path.display().to_string(),
            Outcome::Failed(message) => message.clone(),
        }
    }
}

/// One-line description of what the fetch found.
fn describe_sync(outcome: &Synced) -> String {
    match outcome {
        Synced::UpToDate => "already up to date".to_string(),
        Synced::FastForwarded => "fast-forwarded onto the remote branch".to_string(),
        Synced::LocalAhead => "local commits are waiting to be pushed".to_string(),
        Synced::RemoteBranchMissing => "the remote branch does not exist yet".to_string(),
        Synced::Unavailable(error) => format!("unreachable ({error}); continuing locally"),
    }
}

/// Outcome of the single push at the end of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushReport {
    /// No remote configured, push disabled, nothing to push, or a dry run.
    Skipped(&'static str),
    Pushed,
    Failed(String),
}

/// Everything one `backup run` did.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub started_at: DateTime<Utc>,
    pub elapsed: Duration,
    pub devices: Vec<DeviceReport>,
    pub sync: Option<Synced>,
    pub push: PushReport,
    pub dry_run: bool,
}

impl RunReport {
    pub fn changed(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.outcome.is_change())
            .count()
    }

    pub fn unchanged(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| matches!(d.outcome, Outcome::Unchanged))
            .count()
    }

    pub fn failed(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.outcome.is_failure())
            .count()
    }

    /// A one-line tally for the end of a run.
    pub fn summary(&self) -> String {
        format!(
            "{} device(s): {} changed, {} unchanged, {} failed in {:.1}s{}",
            self.devices.len(),
            self.changed(),
            self.unchanged(),
            self.failed(),
            self.elapsed.as_secs_f64(),
            if self.dry_run { " (dry run)" } else { "" }
        )
    }

    /// Non-zero when any device failed or the push did — suitable for `cron`.
    pub fn exit_code(&self) -> i32 {
        if self.failed() > 0 || matches!(self.push, PushReport::Failed(_)) {
            1
        } else {
            0
        }
    }
}

/// Run the backup pipeline over every device matching `options.filter`.
///
/// Pass `&()` for `progress` when nobody is watching.
///
/// `progress` is generic rather than `&dyn`: a trait object here introduces a
/// higher-ranked lifetime that stops the resulting future from being spawnable.
pub async fn run<P>(config: &Config, options: &RunOptions, progress: &P) -> Result<RunReport>
where
    P: ProgressSink + ?Sized,
{
    let started_at = Utc::now();
    let clock = Instant::now();

    let devices = config.select(&options.filter)?;
    if devices.is_empty() {
        warn!("no devices matched; nothing to do");
        return Ok(RunReport {
            started_at,
            elapsed: clock.elapsed(),
            devices: Vec::new(),
            sync: None,
            push: PushReport::Skipped("no devices matched"),
            dry_run: options.dry_run,
        });
    }

    let remote = config.backup.remote.clone();
    let repo = BackupRepo::open_or_init(&config.backup)?;
    info!(
        repo = %repo.path().display(),
        branch = %repo.branch(),
        devices = devices.len(),
        "starting backup run"
    );

    progress.info(&format!(
        "{} device(s) selected; repository {}",
        devices.len(),
        repo.path().display()
    ));

    let sync = match &remote {
        Some(remote) if !options.dry_run => {
            let outcome = repo.sync(remote)?;
            info!(?outcome, "synced with backup remote");
            progress.info(&format!("remote: {}", describe_sync(&outcome)));
            Some(outcome)
        }
        _ => None,
    };

    let concurrency = options
        .concurrency
        .unwrap_or(config.general.concurrency)
        .max(1);
    let permits = Arc::new(Semaphore::new(concurrency));

    // Captures are pipelined; each result is folded into the repository as soon
    // as it lands, keeping Git work serial without a barrier at the end.
    //
    // The futures are built up front with a plain iterator rather than with
    // `StreamExt::map`: a closure handed to the stream combinator would need a
    // higher-ranked lifetime it cannot have, which makes the whole run future
    // unspawnable. Building them here costs nothing — a future does no work
    // until it is polled.
    let pending: Vec<_> = devices
        .iter()
        .copied()
        .map(|device| capture_one(device, config, Arc::clone(&permits)))
        .collect();
    let mut captures = futures::stream::iter(pending).buffer_unordered(concurrency);

    let mut reports = Vec::with_capacity(devices.len());
    let mut commits = 0usize;

    while let Some((device, result, elapsed)) = captures.next().await {
        let path = device.backup_path(&config.backup.path_template);
        let capture = match result {
            Ok(capture) => capture,
            Err(error) => {
                let message = crate::error::chain(&error.named(&device.name));
                error!(device = %device.name, "{message}");
                let report = DeviceReport {
                    device: device.name.clone(),
                    device_id: device.id,
                    tenant_id: device.tenant_id,
                    host: device.host.clone(),
                    tenant: device.tenant.clone(),
                    path,
                    firmware: None,
                    model: None,
                    identity: None,
                    serial: None,
                    outcome: Outcome::Failed(message),
                    elapsed,
                };
                progress.device(&report);
                reports.push(report);
                continue;
            }
        };

        let outcome = fold_capture(&repo, &path, &capture, options)?;
        if matches!(outcome, Outcome::Committed(_)) {
            commits += 1;
        }
        let report = DeviceReport {
            device: capture.device.clone(),
            device_id: device.id,
            tenant_id: device.tenant_id,
            host: capture.host.clone(),
            tenant: capture.tenant.clone(),
            path,
            firmware: capture.info.version.clone(),
            model: capture.info.model.clone(),
            identity: capture.info.identity.clone(),
            serial: capture.info.serial.clone(),
            outcome,
            elapsed,
        };
        progress.device(&report);
        reports.push(report);
    }

    // Report order follows the config, not completion order, so successive runs
    // are comparable.
    reports.sort_by_key(|report| {
        devices
            .iter()
            .position(|d| d.name == report.device)
            .unwrap_or(usize::MAX)
    });

    let push = push_if_needed(&repo, remote.as_ref(), commits, sync.as_ref(), options);
    match &push {
        PushReport::Pushed => progress.info("pushed to the backup remote"),
        PushReport::Skipped(reason) => progress.info(&format!("push skipped: {reason}")),
        PushReport::Failed(error) => progress.info(&format!("push FAILED: {error}")),
    }

    Ok(RunReport {
        started_at,
        elapsed: clock.elapsed(),
        devices: reports,
        sync,
        push,
        dry_run: options.dry_run,
    })
}

/// Capture one device, holding a concurrency permit for the duration.
///
/// A named function rather than an inline closure: see the note at the call
/// site about higher-ranked lifetimes.
async fn capture_one<'a>(
    device: &'a Device,
    config: &'a Config,
    permits: Arc<Semaphore>,
) -> (
    &'a Device,
    Result<Capture, crate::error::DeviceError>,
    Duration,
) {
    let started = Instant::now();
    let _permit = permits.acquire().await.expect("semaphore is never closed");
    let result = routeros::capture(device, &config.general, &config.export).await;
    (device, result, started.elapsed())
}

/// Write and commit one capture, or report what a dry run would have done.
fn fold_capture(
    repo: &BackupRepo,
    path: &std::path::Path,
    capture: &Capture,
    options: &RunOptions,
) -> Result<Outcome> {
    if options.dry_run {
        return Ok(if repo.would_change(path, &capture.config.contents)? {
            Outcome::WouldChange
        } else {
            Outcome::Unchanged
        });
    }
    Ok(
        match repo.store(path, &capture.config.contents, &commit_meta(capture))? {
            Stored::Unchanged => Outcome::Unchanged,
            Stored::Committed(commit) => Outcome::Committed(commit),
        },
    )
}

fn push_if_needed(
    repo: &BackupRepo,
    remote: Option<&crate::config::Remote>,
    commits: usize,
    sync: Option<&Synced>,
    options: &RunOptions,
) -> PushReport {
    if options.dry_run {
        return PushReport::Skipped("dry run");
    }
    if options.no_push {
        return PushReport::Skipped("--no-push");
    }
    let Some(remote) = remote else {
        return PushReport::Skipped("no backup remote configured");
    };
    if !remote.push {
        return PushReport::Skipped("backup.remote.push = false");
    }
    if commits == 0 {
        // Nothing new from this run, but an earlier run may have been
        // interrupted after committing and before pushing — those commits would
        // otherwise never leave the machine. The fetch at the start of the run
        // already told us, so no second round trip is needed.
        if sync != Some(&Synced::LocalAhead) {
            return PushReport::Skipped("no new commits");
        }
    }
    match repo.push(remote) {
        Ok(()) => PushReport::Pushed,
        Err(error) => {
            error!(%error, "push failed");
            PushReport::Failed(error.to_string())
        }
    }
}

/// Metadata carried into the commit for one capture.
fn commit_meta(capture: &Capture) -> CommitMeta {
    CommitMeta {
        device: capture.device.clone(),
        host: capture.host.clone(),
        tenant: capture.tenant.clone(),
        firmware: capture.info.version.clone(),
        model: capture.info.model.clone(),
        serial: capture.info.serial.clone(),
        software_id: capture.info.software_id.clone(),
        identity: capture.info.identity.clone(),
        command: capture.config.command.clone(),
        captured_at: capture.captured_at,
    }
}

/// Connect to one device and report what it says about itself.
///
/// Backs the "test connection" button and `dondude device test`: credentials and
/// host-key policy can be proven before a fleet run depends on them.
pub async fn test_device(config: &Config, name: &str) -> Result<routeros::RouterInfo> {
    let device: &Device = config
        .find_device(name)
        .ok_or_else(|| Error::config(format!("no device named `{name}`")))?;
    routeros::probe(device, &config.general)
        .await
        .map_err(|error| error.named(&device.name))
}
