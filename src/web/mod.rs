//! The web interface.
//!
//! Layout of this module:
//!
//! * [`routes`] — HTTP handlers, thin by design
//! * [`views`] — server-rendered HTML, all user-facing strings
//! * [`session`] — login cookies and the `Operator` extractor
//! * [`runner`] — background execution of a run, with a live log
//!
//! Every page except `/login` and `/setup` requires an [`Operator`], which the
//! extractor enforces: forgetting to check is not possible, because a handler
//! that does not ask for one has no way to reach an operator's data.
//!
//! [`Operator`]: session::Operator

pub mod routes;
pub mod runner;
pub mod session;
pub mod views;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use chrono::Timelike;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::{Backup, Committer, DeviceFilter, GitAuth};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::git::BackupRepo;
use crate::web::runner::{RunManager, TRIGGER_SCHEDULE};

/// Form extractor. Aliased so a future switch to a custom one (better error
/// pages on malformed input) touches one line rather than every handler.
pub use axum::Form;

/// Shared handler state. Cheap to clone: everything inside is an `Arc` or a
/// small owned value.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    /// Working tree of the backup repository. From the environment, not the
    /// database — it is a mounted volume, not a user preference.
    pub repo_path: PathBuf,
    pub runs: Arc<RunManager>,
}

impl AppState {
    pub fn new(db: Arc<Db>, repo_path: PathBuf) -> Self {
        Self {
            db,
            repo_path,
            runs: Arc::new(RunManager::new()),
        }
    }

    /// Open the backup repository for reading history and diffs.
    ///
    /// libgit2 is blocking, so this runs on the blocking pool rather than
    /// stalling the HTTP worker.
    pub async fn open_repo(&self) -> Result<BackupRepo> {
        let backup = Backup {
            repo_path: self.repo_path.clone(),
            path_template: crate::config::DEFAULT_PATH_TEMPLATE.to_string(),
            committer: Committer::default(),
            remote: None,
        };
        tokio::task::spawn_blocking(move || BackupRepo::open_or_init(&backup))
            .await
            .map_err(|_| Error::config("the repository worker panicked"))?
    }

    /// Check a remote with the given credentials, off the async worker.
    pub async fn probe_remote(
        &self,
        url: &str,
        branch: &str,
        username: &str,
        token: Option<&str>,
        allow_invalid_certs: bool,
    ) -> Result<String> {
        let auth = match token {
            Some(token) => GitAuth::Token {
                username: username.to_string(),
                token: token.to_string(),
            },
            None => GitAuth::None,
        };
        let (url, branch) = (url.to_string(), branch.to_string());
        tokio::task::spawn_blocking(move || {
            crate::git::probe_remote(&url, &branch, &auth, allow_invalid_certs)
        })
        .await
        .map_err(|_| Error::config("the repository worker panicked"))?
    }
}

/// Assemble the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // Public
        .route("/login", get(routes::login_page).post(routes::login))
        .route("/setup", get(routes::setup_page).post(routes::setup))
        .route("/logout", post(routes::logout))
        // Private
        .route("/", get(routes::dashboard))
        .route("/devices", get(routes::devices).post(routes::create_device))
        .route("/devices/new", get(routes::new_device))
        .route("/devices/{id}", post(routes::update_device))
        .route("/devices/{id}/edit", get(routes::edit_device))
        .route("/devices/{id}/delete", post(routes::delete_device))
        .route("/devices/{id}/test", post(routes::test_device))
        .route("/devices/{id}/backup", post(routes::start_device_run))
        .route("/devices/{id}/history", get(routes::device_history))
        .route("/devices/{id}/diff/{commit}", get(routes::device_diff))
        .route("/runs", get(routes::runs).post(routes::start_run))
        .route("/runs/{id}", get(routes::run_detail))
        .route("/api/runs/{id}", get(routes::run_progress))
        .route(
            "/settings",
            get(routes::settings_page).post(routes::save_settings),
        )
        .route("/settings/test", post(routes::test_remote))
        .fallback(routes::not_found)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Serve until the process is asked to stop.
pub async fn serve(bind: &str, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| Error::config(format!("cannot listen on {bind}: {source}")))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind.to_string());
    info!(
        version = crate::VERSION,
        "DonDude is listening on http://{local}"
    );

    // `into_make_service_with_connect_info` is what makes the peer address
    // available to the login throttle.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|source| Error::config(format!("the web server stopped: {source}")))
}

/// Ctrl-C, or SIGTERM from `docker stop`.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutting down");
}

/// Daily scheduled backups.
///
/// Deliberately simple: wake every 30 seconds, compare the clock with the
/// configured time, and refuse to fire twice by asking the database whether a
/// scheduled run already started recently. Storing that in memory instead would
/// re-fire after every restart.
///
/// Times are UTC, so a run does not move twice a year with daylight saving.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            ticker.tick().await;
            if let Err(error) = scheduler_tick(&state).await {
                warn!("{}", crate::error::chain(&error));
            }
        }
    });
}

async fn scheduler_tick(state: &AppState) -> Result<()> {
    let settings = state.db.settings().await?;
    if !settings.schedule_enabled {
        return Ok(());
    }

    let now = chrono::Utc::now();
    if now.hour() as i32 != settings.schedule_hour
        || now.minute() as i32 != settings.schedule_minute
    {
        return Ok(());
    }
    // The minute lasts longer than the tick interval, so check we have not
    // already fired within it.
    if state
        .db
        .scheduled_run_since(now - chrono::Duration::minutes(5))
        .await?
    {
        return Ok(());
    }
    if state.runs.is_running() {
        info!("skipping the scheduled backup: a run is already in progress");
        return Ok(());
    }

    info!("starting the scheduled backup");
    match state
        .runs
        .start(
            state.db.clone(),
            state.repo_path.clone(),
            TRIGGER_SCHEDULE,
            DeviceFilter::default(),
            false,
        )
        .await
    {
        Ok(run_id) => info!(%run_id, "scheduled backup started"),
        Err(error) => error!("{}", crate::error::chain(&error)),
    }
    Ok(())
}


/// Background device-state monitoring.
///
/// Same shape as the scheduler: wake, read settings, decide. Polling only
/// happens while `monitor_enabled` is on, so the operator's toggle in Settings
/// takes effect within one interval without a restart. The interval itself is
/// read fresh every tick for the same reason.
///
/// Retention pruning runs alongside polling, but rarely: once a day is plenty,
/// and pruning eagerly after every sweep would delete nothing 143 times out of
/// 144 while holding a write lock.
pub fn spawn_monitor(state: AppState) {
    tokio::spawn(async move {
        let mut pruned = chrono::Utc::now().date_naive();
        loop {
            let wait = match monitor_tick(state.clone()).await {
                Ok(interval) => interval,
                Err(error) => {
                    tracing::warn!("{}", crate::error::chain(&error));
                    60
                }
            };
            if chrono::Utc::now().date_naive() != pruned {
                pruned = chrono::Utc::now().date_naive();
                if let Ok(settings) = state.db.settings().await {
                    match state.db.prune_samples(settings.monitor_retention_days).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(n, "pruned old monitor samples"),
                        Err(error) => {
                            tracing::warn!("{}", crate::error::chain(&error))
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(wait.max(1) as u64)).await;
        }
    });
}

/// One sweep, or nothing if monitoring is off. Returns the seconds to wait
/// before the next tick.
async fn monitor_tick(state: AppState) -> Result<u32> {
    let settings = state.db.settings().await?;
    if !settings.monitor_enabled {
        return Ok(30);
    }
    let interval = settings.monitor_interval_secs.max(10);

    let config = state
        .db
        .runtime_config(state.repo_path.clone())
        .await?;
    let report = crate::monitor::poll_fleet(&state.db, &config).await;
    for failure in &report.failures {
        tracing::debug!(device = %failure.device, %failure.error, "monitor poll failed");
    }
    Ok(interval as u32)
}
