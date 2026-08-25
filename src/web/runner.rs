//! Background execution of backup runs, with a live log for the browser.
//!
//! A run is started from an HTTP handler but outlives the request, so it is
//! spawned as a task and observed through [`RunManager`]. The browser polls a
//! small JSON endpoint for progress rather than holding a connection open — no
//! websockets, no server-sent events, nothing to reconnect after a hiccup.
//!
//! **One run at a time.** Two concurrent runs would race on the Git index and
//! interleave commits, so [`RunManager::start`] refuses while one is in flight.
//! That refusal is the whole reason this type holds state instead of being a
//! bare function.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tracing::{error, info};
use uuid::Uuid;

use crate::backup::{self, DeviceReport, ProgressSink, RunOptions};
use crate::config::DeviceFilter;
use crate::db::{Db, RunLock};
use crate::error::{Error, Result};

/// How the run was triggered. Stored on the run row.
pub const TRIGGER_MANUAL: &str = "manual";
pub const TRIGGER_SCHEDULE: &str = "schedule";
pub const TRIGGER_CLI: &str = "cli";

/// A snapshot of the run currently in flight, safe to hand to a template.
#[derive(Debug, Clone)]
pub struct Live {
    pub id: Uuid,
    pub started_at: DateTime<Utc>,
    pub trigger: String,
    pub log: Vec<String>,
    pub finished: bool,
    pub failed: bool,
    /// Set once the run ends: the tally, or the error that stopped it.
    pub summary: Option<String>,
}

#[derive(Default)]
struct Shared {
    current: Option<Live>,
}

/// Owns the single in-flight run and its log.
#[derive(Default)]
pub struct RunManager {
    shared: Mutex<Shared>,
}

impl RunManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current or most recent run, if this process has seen one.
    pub fn latest(&self) -> Option<Live> {
        self.lock().current.clone()
    }

    /// The live view of one run, if it is the one this process is tracking.
    ///
    /// Older runs are read from the database instead: their log was persisted
    /// when they finished.
    pub fn snapshot(&self, id: Uuid) -> Option<Live> {
        self.lock().current.clone().filter(|live| live.id == id)
    }

    pub fn is_running(&self) -> bool {
        self.lock()
            .current
            .as_ref()
            .is_some_and(|live| !live.finished)
    }

    /// Start a run in the background and return its id immediately.
    ///
    /// Fails if a run is already in flight — whether this process started it or
    /// a `dondude backup run` elsewhere did.
    pub async fn start(
        self: &Arc<Self>,
        db: Arc<Db>,
        repo_path: PathBuf,
        trigger: &str,
        filter: DeviceFilter,
        dry_run: bool,
    ) -> Result<Uuid> {
        // Claim the slot before touching the database, so two simultaneous
        // requests cannot both create a run row.
        {
            let mut shared = self.lock();
            if shared.current.as_ref().is_some_and(|live| !live.finished) {
                return Err(Error::config(
                    "a backup run is already in progress; wait for it to finish",
                ));
            }
            shared.current = Some(Live {
                id: Uuid::nil(),
                started_at: Utc::now(),
                trigger: trigger.to_string(),
                log: vec![stamp("starting")],
                finished: false,
                failed: false,
                summary: None,
            });
        }

        // The in-process slot only covers this process. The advisory lock is what
        // keeps a cron-driven run from interleaving commits with this one.
        let run_lock = match db.try_lock_run().await {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.lock().current = None;
                return Err(Error::config(
                    "a backup run is already in progress (it may have been started from the \
                     command line); wait for it to finish",
                ));
            }
            Err(error) => {
                self.lock().current = None;
                return Err(error);
            }
        };

        let run_id = match db.start_run(trigger, dry_run).await {
            Ok(id) => id,
            Err(error) => {
                // Release the slot we just claimed, or nothing could ever run.
                // `run_lock` drops here, which releases the advisory lock too.
                self.lock().current = None;
                return Err(error);
            }
        };
        if let Some(live) = self.lock().current.as_mut() {
            live.id = run_id;
        }

        tokio::spawn(execute(
            Arc::clone(self),
            db,
            repo_path,
            run_id,
            filter,
            dry_run,
            run_lock,
        ));
        Ok(run_id)
    }

    fn push_line(&self, line: String) {
        if let Some(live) = self.lock().current.as_mut() {
            live.log.push(line);
        }
    }

    fn log_text(&self) -> String {
        self.lock()
            .current
            .as_ref()
            .map(|live| live.log.join("\n"))
            .unwrap_or_default()
    }

    fn finish(&self, summary: &str, failed: bool) {
        if let Some(live) = self.lock().current.as_mut() {
            live.finished = true;
            live.failed = failed;
            live.summary = Some(summary.to_string());
        }
    }

    /// A poisoned lock means a previous holder panicked while only ever pushing
    /// log lines; the data is still consistent, so recover rather than abort.
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The run itself.
///
/// A free function rather than a method so the spawned future has no
/// higher-ranked lifetimes to satisfy. Whatever happens, the run is marked
/// finished — otherwise the UI would sit at "in progress" forever.
async fn execute(
    manager: Arc<RunManager>,
    db: Arc<Db>,
    repo_path: PathBuf,
    run_id: Uuid,
    filter: DeviceFilter,
    dry_run: bool,
    // Held for the whole run and dropped on the way out, which is what releases
    // the advisory lock. Nothing reads it.
    _run_lock: RunLock,
) {
    let sink = LogSink {
        manager: Arc::clone(&manager),
    };

    let outcome = async {
        let config = db.runtime_config(repo_path).await?;
        let options = RunOptions {
            filter,
            dry_run,
            no_push: false,
            concurrency: None,
        };
        backup::run(&config, &options, &sink).await
    }
    .await;

    match outcome {
        Ok(report) => {
            let summary = report.summary();
            sink.info(&summary);
            let log = manager.log_text();
            if let Err(error) = db.finish_run(run_id, &report, &log).await {
                error!(%error, "could not record the run");
            }
            manager.finish(&summary, report.exit_code() != 0);
            info!(%run_id, "{summary}");
        }
        Err(error) => {
            let message = crate::error::chain(&error);
            sink.info(&format!("run aborted: {message}"));
            let log = manager.log_text();
            if let Err(error) = db.abort_run(run_id, &message, &log).await {
                error!(%error, "could not record the aborted run");
            }
            manager.finish(&format!("aborted: {message}"), true);
            error!(%run_id, "run aborted: {message}");
        }
    }
}

/// Bridges the pipeline's progress reports into the live log.
struct LogSink {
    manager: Arc<RunManager>,
}

impl ProgressSink for LogSink {
    fn info(&self, message: &str) {
        self.manager.push_line(stamp(message));
    }

    fn device(&self, report: &DeviceReport) {
        self.manager.push_line(stamp(&format!(
            "{}: {} — {}",
            report.device,
            report.outcome.label(),
            report.detail()
        )));
    }
}

fn stamp(message: &str) -> String {
    format!("{} {message}", Utc::now().format("%H:%M:%S"))
}

/// Report totals in the shape the JSON progress endpoint returns.
pub fn progress_json(live: &Live) -> serde_json::Value {
    serde_json::json!({
        "id": live.id,
        "finished": live.finished,
        "failed": live.failed,
        "summary": live.summary,
        "log": live.log,
    })
}
