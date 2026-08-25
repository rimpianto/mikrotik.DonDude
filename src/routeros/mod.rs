//! MikroTik RouterOS communicator.
//!
//! Phase 1 speaks SSH only. The RouterOS binary API (port 8728/8729) is the
//! planned second transport; [`Transport`] exists so the backup pipeline can be
//! written against a capability rather than against `libssh2`.
//!
//! ## Blocking inside async
//!
//! `libssh2` is synchronous and its session types are not `Send`-safe to hold
//! across await points, so a whole device conversation — connect, authenticate,
//! run commands, disconnect — happens inside a single
//! `tokio::task::spawn_blocking`. Concurrency comes from running many such
//! tasks, bounded by `general.concurrency`.

pub mod export;
pub mod ssh;

use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::config::{Device, Export, General};
use crate::error::DeviceError;

pub use export::{ExportedConfig, RouterInfo};
pub use ssh::{CommandOutput, SshSession, Target};

/// Metadata commands. All three are best-effort: a device that refuses one
/// still gets backed up, just with a thinner header.
const CMD_IDENTITY: &str = "/system identity print";
const CMD_RESOURCE: &str = "/system resource print";
/// Absent on CHR and x86 installs, where it fails harmlessly.
const CMD_ROUTERBOARD: &str = "/system routerboard print";

/// How a device is reached. SSH today; `Api` is reserved for the binary API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Ssh,
}

/// One successful capture from one device.
#[derive(Debug, Clone)]
pub struct Capture {
    pub device: String,
    pub host: String,
    pub tenant: String,
    pub transport: Transport,
    pub info: RouterInfo,
    pub config: ExportedConfig,
    pub captured_at: DateTime<Utc>,
}

impl Capture {
    /// Firmware string for commit metadata, or `unknown` if the device did not
    /// tell us.
    pub fn firmware(&self) -> &str {
        self.info.version.as_deref().unwrap_or("unknown")
    }
}

/// Connect to `device`, run `/export`, and return a normalized capture.
///
/// Metadata gathering never fails the capture; only the export itself can.
pub async fn capture(
    device: &Device,
    general: &General,
    fleet_export: &Export,
) -> Result<Capture, DeviceError> {
    let target = Target::from_config(device, general);
    let options = fleet_export.clone();
    let command = options.command_line();
    let name = device.name.clone();
    let host = device.host.clone();
    let tenant = device.tenant.clone();

    // Outer guard for the whole conversation. `libssh2`'s own timeout bounds
    // each read; this bounds the sum of them, including DNS and TCP setup.
    let budget = general.connect_timeout() + general.command_timeout() * 4;
    let blocking_command = command.clone();
    let blocking_name = name.clone();

    let work = tokio::task::spawn_blocking(move || {
        let session = SshSession::connect(target)?;
        let info = collect_info(&session);
        let raw = session.exec_checked(&blocking_command)?;

        let mut info = info;
        // Banner values only fill gaps the `/system` prints left behind.
        info.merge_from(RouterInfo::from_export_banner(&raw));
        let contents = export::render(&raw, &blocking_command, &blocking_name, &info, &options);

        Ok::<_, DeviceError>((
            info.clone(),
            ExportedConfig {
                contents,
                info,
                command: blocking_command,
            },
        ))
    });

    let (info, config) = with_timeout(work, budget).await?;

    debug!(
        device = %name,
        bytes = config.contents.len(),
        firmware = info.version.as_deref().unwrap_or("unknown"),
        "captured export"
    );

    Ok(Capture {
        device: name,
        host,
        tenant,
        transport: Transport::Ssh,
        info,
        config,
        captured_at: Utc::now(),
    })
}

/// Connect and read identity/version without exporting anything.
///
/// Used by `dondude device test` to check credentials and reachability.
pub async fn probe(device: &Device, general: &General) -> Result<RouterInfo, DeviceError> {
    let target = Target::from_config(device, general);
    let budget = general.connect_timeout() + general.command_timeout();
    let work = tokio::task::spawn_blocking(move || {
        let session = SshSession::connect(target)?;
        Ok::<_, DeviceError>(collect_info(&session))
    });
    with_timeout(work, budget).await
}

/// Best-effort device facts. Each command that fails is logged and skipped.
fn collect_info(session: &SshSession) -> RouterInfo {
    let mut info = RouterInfo::default();
    for (command, parse) in [
        (
            CMD_RESOURCE,
            RouterInfo::from_resource_print as fn(&str) -> RouterInfo,
        ),
        (CMD_IDENTITY, RouterInfo::from_identity_print),
        (CMD_ROUTERBOARD, RouterInfo::from_routerboard_print),
    ] {
        match session.exec(command) {
            Ok(output) if output.status == 0 => info.merge_from(parse(&output.stdout)),
            Ok(output) => debug!(%command, status = output.status, "metadata command refused"),
            Err(error) => debug!(%command, %error, "metadata command failed"),
        }
    }
    info
}

/// Await a blocking task with a wall-clock ceiling.
///
/// A timeout abandons the task rather than killing it: a blocking thread cannot
/// be cancelled, so it runs until `libssh2` gives up on its own read timeout.
/// The pipeline is freed immediately either way.
async fn with_timeout<T>(
    handle: tokio::task::JoinHandle<Result<T, DeviceError>>,
    budget: Duration,
) -> Result<T, DeviceError> {
    match tokio::time::timeout(budget, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => {
            warn!(%join_error, "device worker did not finish cleanly");
            Err(DeviceError::WorkerPanic)
        }
        Err(_) => Err(DeviceError::Timeout(budget)),
    }
}
