//! Phase 5: RouterOS and routerboard-firmware update checking.
//!
//! The read-only half of firmware management. For each device it answers two
//! questions against the release channel the device is attached to:
//!
//! * is the installed RouterOS older than what the channel offers?
//! * is the flashed routerboard firmware older than what the installed
//!   packages ship? (CHR and x86 installs have no routerboard firmware.)
//!
//! The check reuses the backup/monitor SSH plumbing — one credential store,
//! one host-key policy, one failure vocabulary — and never writes to a
//! router. Results are upserted into `device_updates` (one row per device)
//! for the web UI; a failed check keeps the last good versions and only
//! records `last_error`, the same way a failed backup never blanks the
//! known firmware.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use tracing::{debug, warn};

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;

/// Refreshes `latest-version` from the channel. Best-effort: without
/// internet the print below still reports the installed facts.
const CMD_CHECK: &str = "/system package update check-for-updates";
/// Reports `channel`, `status`, `installed-version`, `latest-version`.
const CMD_PACKAGE_PRINT: &str = "/system package update print";
/// Absent on CHR and x86 installs, where it fails harmlessly.
const CMD_ROUTERBOARD_PRINT: &str = "/system routerboard print";

/// What one device told us about its update state, channel-relative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStatus {
    /// RouterOS release channel the device follows (`stable`, ...).
    pub channel: Option<String>,
    /// Installed RouterOS version.
    pub installed_version: Option<String>,
    /// Latest version the channel offered at the last check.
    pub latest_version: Option<String>,
    /// Raw device status line, e.g. `System is up to date`.
    pub status: Option<String>,
    /// Routerboard firmware currently flashed.
    pub fw_current: Option<String>,
    /// Routerboard firmware bundled with the installed packages.
    pub fw_upgrade: Option<String>,
    /// Whether the device has routerboard firmware at all (false on CHR/x86).
    pub firmware_supported: bool,
}

impl UpdateStatus {
    /// True when the channel offers a newer RouterOS build than installed.
    pub fn ros_update_available(&self) -> bool {
        matches!((&self.installed_version, &self.latest_version), (Some(a), Some(b)) if a != b)
    }

    /// True when the installed packages ship newer routerboard firmware
    /// than what is flashed.
    pub fn fw_update_available(&self) -> bool {
        self.firmware_supported
            && matches!((&self.fw_current, &self.fw_upgrade), (Some(a), Some(b)) if a != b)
    }
}

/// Parse `/system package update print` output.
pub fn from_package_update_print(raw: &str) -> UpdateStatus {
    let f = crate::routeros::export::parse_print(raw);
    UpdateStatus {
        channel: non_empty(f.get("channel")),
        installed_version: non_empty(f.get("installed-version")),
        latest_version: non_empty(f.get("latest-version")),
        status: non_empty(f.get("status")),
        ..UpdateStatus::default()
    }
}

/// Merge `/system routerboard print` output into the status.
///
/// Real hardware reports `current-firmware`/`upgrade-firmware`, which marks
/// the device firmware-supported. A refusal (CHR, x86) never reaches here,
/// leaving the status untouched.
pub fn merge_routerboard_print(status: &mut UpdateStatus, raw: &str) {
    let f = crate::routeros::export::parse_print(raw);
    if let Some(v) = non_empty(f.get("current-firmware")) {
        status.fw_current = Some(v);
    }
    if let Some(v) = non_empty(f.get("upgrade-firmware")) {
        status.fw_upgrade = Some(v);
    }
    status.firmware_supported = status.fw_current.is_some() || status.fw_upgrade.is_some();
}

fn non_empty(value: Option<&String>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One successful check, tied back to its device.
#[derive(Debug, Clone)]
pub struct DeviceCheck {
    pub device_id: uuid::Uuid,
    pub device: String,
    pub tenant: String,
    pub checked_at: DateTime<Utc>,
    pub status: UpdateStatus,
}

/// Why one device could not be checked.
#[derive(Debug, Clone)]
pub struct CheckFailure {
    pub device: String,
    pub error: String,
}

/// The outcome of one fleet-wide sweep.
#[derive(Debug, Default)]
pub struct CheckReport {
    pub checks: Vec<DeviceCheck>,
    pub failures: Vec<CheckFailure>,
}

impl CheckReport {
    pub fn describe(&self) -> String {
        format!(
            "{} checked, {} unreachable",
            self.checks.len(),
            self.failures.len()
        )
    }
}

/// Check the whole enabled fleet once, persisting every outcome.
pub async fn check_fleet(db: &Db, config: &Config) -> CheckReport {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config.general.concurrency.max(1),
    ));
    let general = config.general.clone();

    let enabled: Vec<crate::config::Device> = config
        .devices
        .iter()
        .filter(|d| d.enabled)
        .cloned()
        .collect();
    let mut tasks = futures::stream::iter(enabled)
        .map(|device| {
            let permit = semaphore.clone();
            let general = general.clone();
            async move {
                let _permit = permit.acquire_owned().await;
                let id = device.id;
                let name = device.name.clone();
                let tenant = device.tenant.clone();
                match check_device_inner(&device, &general).await {
                    Ok(status) => Ok(DeviceCheck {
                        device_id: id,
                        device: name,
                        tenant,
                        checked_at: Utc::now(),
                        status,
                    }),
                    Err(error) => Err((id, name, crate::error::chain(&error).to_string())),
                }
            }
        })
        .buffer_unordered(config.general.concurrency.max(1));

    let mut report = CheckReport::default();
    while let Some(outcome) = tasks.next().await {
        match outcome {
            Ok(check) => {
                debug!(device = %check.device, channel = ?check.status.channel, "checked for updates");
                if let Err(error) = db
                    .upsert_device_update(check.device_id, Some(&check.status), None)
                    .await
                {
                    warn!(%error, "storing update check failed");
                }
                report.checks.push(check);
            }
            Err((id, name, error)) => {
                warn!(device = %name, %error, "update check failed");
                if let Err(store_error) = db.upsert_device_update(id, None, Some(&error)).await {
                    warn!(%store_error, "storing update-check failure failed");
                }
                report.failures.push(CheckFailure {
                    device: name,
                    error,
                });
            }
        }
    }
    report
}

/// Check one device: connect, ask its channel, disconnect.
async fn check_device_inner(
    device: &crate::config::Device,
    general: &crate::config::General,
) -> Result<UpdateStatus> {
    let target = crate::routeros::Target::from_config(device, general);
    let device_name = device.name.clone();

    let budget = general.connect_timeout() + general.command_timeout() * 3;
    let work = tokio::task::spawn_blocking(move || {
        let session = crate::routeros::SshSession::connect(target).map_err(|source| {
            crate::error::Error::Device {
                name: device_name.clone(),
                source,
            }
        })?;
        // Refresh latest-version; a router without internet still reports
        // its installed facts, so the refresh is best-effort.
        if let Err(error) = session.exec(CMD_CHECK) {
            debug!(%error, "check-for-updates refused (offline device?)");
        }
        let package = session.exec_checked(CMD_PACKAGE_PRINT).map_err(|source| {
            crate::error::Error::Device {
                name: device_name.clone(),
                source,
            }
        })?;
        // Optional: absent on CHR/x86, where it fails harmlessly.
        let routerboard = match session.exec(CMD_ROUTERBOARD_PRINT) {
            Ok(output) if output.status == 0 => Some(output.stdout),
            _ => None,
        };
        Ok::<_, crate::error::Error>((package, routerboard))
    });

    let (package, routerboard) = match tokio::time::timeout(budget, work).await {
        Ok(Ok(pair)) => pair?,
        Ok(Err(_)) => {
            return Err(crate::error::Error::config(
                "the update-check worker did not finish cleanly",
            ));
        }
        Err(_) => return Err(crate::error::Error::config("update check timed out")),
    };

    let mut status = from_package_update_print(&package);
    if let Some(raw) = routerboard.as_deref() {
        merge_routerboard_print(&mut status, raw);
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP_TO_DATE: &str = " channel: stable\n status: System is up to date\n installed-version: 7.24.4\n latest-version: 7.24.4\n";
    const NEW_AVAILABLE: &str = " channel: stable\n status: System is not up to date\n installed-version: 7.24.4\n latest-version: 7.25\n";
    const ROUTERBOARD: &str =
        " board-name: RB5009UG+S+\n current-firmware: 7.24.3\n upgrade-firmware: 7.24.4\n";

    #[test]
    fn parses_the_package_update_print() {
        let s = from_package_update_print(NEW_AVAILABLE);
        assert_eq!(s.channel.as_deref(), Some("stable"));
        assert_eq!(s.installed_version.as_deref(), Some("7.24.4"));
        assert_eq!(s.latest_version.as_deref(), Some("7.25"));
        assert!(s.ros_update_available());
        assert!(!s.fw_update_available());
    }

    #[test]
    fn an_up_to_date_device_reports_no_ros_update() {
        let s = from_package_update_print(UP_TO_DATE);
        assert!(!s.ros_update_available());
    }

    #[test]
    fn routerboard_firmware_mismatch_is_detected() {
        let mut s = from_package_update_print(UP_TO_DATE);
        merge_routerboard_print(&mut s, ROUTERBOARD);
        assert!(s.firmware_supported);
        assert_eq!(s.fw_current.as_deref(), Some("7.24.3"));
        assert_eq!(s.fw_upgrade.as_deref(), Some("7.24.4"));
        assert!(s.fw_update_available());
    }

    #[test]
    fn matching_firmware_reports_no_update() {
        let mut s = from_package_update_print(UP_TO_DATE);
        merge_routerboard_print(
            &mut s,
            " board-name: RB5009UG+S+\n current-firmware: 7.24.4\n upgrade-firmware: 7.24.4\n",
        );
        assert!(!s.fw_update_available());
    }

    #[test]
    fn a_device_without_routerboard_output_stays_unsupported() {
        // CHR/x86: the routerboard command is refused and never merged, or
        // the output simply lacks the firmware fields.
        let mut s = from_package_update_print(UP_TO_DATE);
        merge_routerboard_print(&mut s, " board-name: x86_64\n cpu: Intel\n");
        assert!(!s.firmware_supported);
        assert!(!s.fw_update_available());
    }
}
