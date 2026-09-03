//! Phase 2: RouterOS state monitoring.
//!
//! A light-touch sampler, deliberately unlike the backup pipeline. A backup is
//! allowed to open a session, take its time and shut down; monitoring wants to
//! *sit* on a fleet for weeks. So the poller:
//!
//! * reuses the same SSH [`Target`] and `SshSession` as the backups — one
//!   credential store, one host-key policy, one failure vocabulary;
//! * never lets one device break the loop: a failed sample logs, records the
//!   failure in memory for the next poll's backoff, and moves on;
//! * writes only successful samples to `device_samples` — the DB stores what
//!   the routers said, not what we tried;
//! * keeps no per-device task alive between polls. One tick, one sweep of the
//!   fleet, bounded by the same `general.concurrency` semaphore the backups
//!   use. A router wedged mid-command cannot hold a slot past the timeout.
//!
//! ## Why no per-device timer
//!
//! Spawning one long-lived connection per device would show "live" numbers, but
//! SSH sessions to RouterOS do not survive idle periods reliably and a stalled
//! session blocks that device's samples silently. Poll-sweep-reconnect is
//! cruder and one line of code, and the sweep cost is one `/system resource
//! print` per device per interval.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;

/// Commands sampled on each poll. `/system health print` is absent on many
/// boards (CHR, x86), where it fails harmlessly.
const CMD_RESOURCE: &str = "/system resource print";
const CMD_HEALTH: &str = "/system health print";

/// One sample from one device.
///
/// `cpu_load` etc. mirror the `device_samples` columns; `extra` carries values
/// this slice does not graph, so a later feature can extend without a
/// migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub device_id: uuid::Uuid,
    pub device: String,
    pub tenant: String,
    pub captured_at: DateTime<Utc>,
    /// 0-100, percent busy as the device reports it.
    pub cpu_load: Option<i32>,
    pub free_memory: Option<i64>,
    pub total_memory: Option<i64>,
    pub free_hdd: Option<i64>,
    pub total_hdd: Option<i64>,
    pub uptime_secs: Option<i64>,
    /// Volts, as printed by `/system health print`.
    pub voltage: Option<f64>,
    /// Degrees C, as printed by `/system health print`.
    pub temperature: Option<f64>,
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Why a poll skipped a device. Not persisted — the next tick retries.
#[derive(Debug, Clone)]
pub struct PollFailure {
    pub device: String,
    pub error: String,
}

/// Result of one sweep of the fleet.
#[derive(Debug, Clone, Default)]
pub struct PollReport {
    pub samples: Vec<Sample>,
    pub failures: Vec<PollFailure>,
}

impl PollReport {
    pub fn describe(&self) -> String {
        format!(
            "{} sampled, {} unreachable",
            self.samples.len(),
            self.failures.len()
        )
    }
}

/// Poll the whole enabled fleet once. Used by the monitor loop, by
/// `dondude monitor poll` (one-shot), and by tests.
pub async fn poll_fleet(db: &Db, config: &Config) -> PollReport {
    use futures::StreamExt;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        config.general.concurrency.max(1),
    ));
    let general = config.general.clone();

    // Collect enabled devices first: borrowing into an async closure trips
    // rustc's higher-ranked lifetime check on `buffer_unordered`.
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
                sample_device(&device, &general).await
            }
        })
        .buffer_unordered(config.general.concurrency.max(1));

    let mut report = PollReport::default();
    while let Some(outcome) = tasks.next().await {
        match outcome {
            Ok(sample) => report.samples.push(sample),
            Err((name, error)) => report.failures.push(PollFailure {
                device: name,
                error: crate::error::chain(&error).to_string(),
            }),
        }
    }

    // Store first, then publish — a store failure is a real error, not a
    // cosmetic one. Failures here are logged; the caller decides whether to
    // retry the whole sweep.
    if let Err(error) = db.insert_samples(&report.samples).await {
        warn!(%error, "storing monitor samples failed");
    }
    report
}

/// Sample one device: connect, read resource + health, disconnect.
async fn sample_device(
    device: &crate::config::Device,
    general: &crate::config::General,
) -> std::result::Result<Sample, (String, crate::error::Error)> {
    let device_name = device.name.clone();
    match sample_device_inner(device, general).await {
        Ok(sample) => Ok(sample),
        Err(error) => Err((device_name, error)),
    }
}

async fn sample_device_inner(
    device: &crate::config::Device,
    general: &crate::config::General,
) -> Result<Sample> {
    let target = crate::routeros::Target::from_config(device, general);
    let device_name = device.name.clone();
    let name = device.name.clone();
    let tenant = device.tenant.clone();

    let budget = general.connect_timeout() + general.command_timeout() * 3;
    let work = tokio::task::spawn_blocking(move || {
        let session = crate::routeros::SshSession::connect(target).map_err(|source| {
            crate::error::Error::Device {
                name: device_name.clone(),
                source,
            }
        })?;
        let resource =
            session
                .exec_checked(CMD_RESOURCE)
                .map_err(|source| crate::error::Error::Device {
                    name: device_name.clone(),
                    source,
                })?;
        // Health is optional hardware; a refusal is not a failure.
        let health = match session.exec(CMD_HEALTH) {
            Ok(output) if output.status == 0 => Some(output.stdout),
            _ => None,
        };
        Ok::<_, crate::error::Error>((resource, health))
    });
    let (resource, health) = match tokio::time::timeout(budget, work).await {
        Ok(Ok(pair)) => pair?,
        Ok(Err(_)) => {
            return Err(crate::error::Error::config(
                "the sampling worker did not finish cleanly",
            ));
        }
        Err(_) => return Err(crate::error::Error::config("sampling timed out")),
    };

    let sample = parse_sample(&resource, health.as_deref());
    debug!(device = %name, cpu = ?sample.cpu_load, "sampled device");
    Ok(Sample {
        device_id: device.id,
        device: name,
        tenant,
        captured_at: Utc::now(),
        ..sample
    })
}

/// Turn `/system resource print` (+ optional health) output into a [`Sample`]
/// with only the standard columns filled. Exposed for tests.
pub fn parse_sample(resource: &str, health: Option<&str>) -> Sample {
    let r = parse_print(resource);
    let h = health.map(parse_health).unwrap_or_default();

    let health_value = |key: &str, fields: &BTreeMap<String, String>| -> Option<f64> {
        fields.get(key).and_then(|v| {
            // "24.5V" / "47C" / "45 C" — strip any unit suffix, then spaces.
            let trimmed = v.trim().trim_end_matches(['V', 'C', 'v', 'c']).trim_end();
            let first = trimmed.split_whitespace().next().unwrap_or("");
            first.parse::<f64>().ok()
        })
    };

    Sample {
        device_id: uuid::Uuid::nil(),
        device: String::new(),
        tenant: String::new(),
        captured_at: Utc::now(),
        cpu_load: r
            .get("cpu-load")
            .and_then(|v| v.trim().trim_end_matches('%').parse().ok()),
        free_memory: r.get("free-memory").and_then(|v| parse_bytes(v)),
        total_memory: r.get("total-memory").and_then(|v| parse_bytes(v)),
        free_hdd: r.get("free-hdd-space").and_then(|v| parse_bytes(v)),
        total_hdd: r.get("total-hdd-space").and_then(|v| parse_bytes(v)),
        uptime_secs: r.get("uptime").and_then(|v| parse_uptime(v)),
        voltage: health_value("voltage", &h).or_else(|| health_value("voltage2", &h)),
        temperature: ["cpu-temperature", "temperature", "board-temperature1"]
            .iter()
            .find_map(|k| health_value(k, &h)),
        extra: serde_json::json!({
            "architecture": r.get("architecture-name"),
            "board_name": r.get("board-name"),
            "version": r.get("version"),
        }),
    }
}

/// `/system health print` on RouterOS 7 is a columnar table, e.g.
/// `0  cpu-temperature        45  C`. Extract every `name value unit` row;
/// fall back to `key: value` lines for older releases that print pairs.
fn parse_health(raw: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in raw.lines().skip_while(|l| !l.contains("NAME")) {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("NAME")
            || line.starts_with('#') && !line.contains("  ")
        {
            // header line: "# NAME VALUE TYPE" — skip
        }
        if line.starts_with('#') || line.starts_with("NAME") || line.is_empty() {
            continue;
        }
        // "0  cpu-temperature  45  C" -> name=cpu-temperature, value=45C
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[0].chars().all(|c| c.is_ascii_digit()) {
            let name = parts[1];
            let value = parts[2..].join(" ");
            fields.insert(name.to_string(), value);
        }
    }
    // Older releases use key: value; merge over anything columnar.
    for (k, v) in parse_print(raw) {
        fields.insert(k, v);
    }
    fields
}

/// RouterOS `print` output: `key: value` lines. Shared shape with the export
/// banner parser, reimplemented here to keep `routeros::export` private.
fn parse_print(raw: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        fields.insert(key, value.trim().to_string());
    }
    fields
}

/// Parse "1575MiB" / "1024KiB" / "1GiB" into bytes.
fn parse_bytes(value: &str) -> Option<i64> {
    let value = value.trim();
    let (num, unit) = value.split_at(
        value
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(value.len()),
    );
    let num: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult) as i64)
}

/// Parse RouterOS uptime "1w2d3h4m5s" into seconds.
pub fn parse_uptime(value: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut num = String::new();
    let mut any = false;
    for c in value.trim().chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num.parse().ok()?;
            num.clear();
            let secs = match c {
                'w' => n * 7 * 24 * 3600,
                'd' => n * 24 * 3600,
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => return None,
            };
            total += secs;
            any = true;
        }
    }
    if any { Some(total) } else { None }
}

use std::collections::BTreeMap;

#[cfg(test)]
mod tests {
    use super::*;

    const RESOURCE: &str = "\
                   uptime: 1w2d3h4m5s
                   version: 7.15.3 (stable)
                   build-time: unknown
                   free-memory: 405MiB
                   total-memory: 1024MiB
                   cpu: ARM
                   cpu-count: 1
                   cpu-load: 23
                   free-hdd-space: 9504KiB
                   total-hdd-space: 16384KiB
                   architecture-name: arm
                   board-name: hAP ax2
";

    const HEALTH: &str = "\
                   voltage: 24.5V
                   temperature: 47C
";

    #[test]
    fn parses_resource_print() {
        let s = parse_sample(RESOURCE, None);
        assert_eq!(s.cpu_load, Some(23));
        assert_eq!(s.free_memory, Some(405 * 1024 * 1024));
        assert_eq!(s.total_memory, Some(1024 * 1024 * 1024));
        assert_eq!(s.free_hdd, Some(9504 * 1024));
        assert_eq!(s.total_hdd, Some(16384 * 1024));
        assert_eq!(s.uptime_secs, Some((7 + 2) * 86400 + 3 * 3600 + 4 * 60 + 5));
    }

    #[test]
    fn parses_health_when_present() {
        let s = parse_sample(RESOURCE, Some(HEALTH));
        assert_eq!(s.voltage, Some(24.5));
        assert_eq!(s.temperature, Some(47.0));
    }

    #[test]
    fn health_missing_leaves_none() {
        let s = parse_sample(RESOURCE, None);
        assert_eq!(s.voltage, None);
        assert_eq!(s.temperature, None);
    }

    #[test]
    fn uptime_variants() {
        assert_eq!(parse_uptime("5s"), Some(5));
        assert_eq!(parse_uptime("2m30s"), Some(150));
        assert_eq!(parse_uptime("1h"), Some(3600));
        assert_eq!(parse_uptime("10d"), Some(864000));
        assert_eq!(parse_uptime(""), None);
    }

    #[test]
    fn bytes_variants() {
        assert_eq!(parse_bytes("405MiB"), Some(424_673_280));
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("weird"), None);
    }
}

#[cfg(test)]
mod knot_parse_tests {

    #[test]
    fn parse_knot_like_output() {
        let resource = "uptime: 3h13m50s           \n                  version: 7.24.1 (stable)    \n               free-memory: 49.7MiB            \n             total-memory: 256.0MiB           \n                      cpu: ARM                \n                cpu-count: 1                  \n                 cpu-load: 23%                \n           free-hdd-space: 425.5MiB           \n             total-hdd-space: 512.0MiB         \n          bad-blocks: 0%                 \n        architecture-name: arm                \n          board-name: KNOT Embedded LTE4 \n";
        let health = "Columns: NAME, VALUE, TYPE\n#  NAME                VALUE  TYPE\n0  cpu-temperature        45  C   \n1  board-temperature1     40  C   \n";
        let s = crate::monitor::parse_sample(resource, Some(health));
        assert_eq!(s.cpu_load, Some(23));
        assert_eq!(s.temperature, Some(45.0));
        assert!(s.free_memory.is_some());
    }
}
