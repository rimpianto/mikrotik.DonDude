//! HTTP handlers.
//!
//! Handlers stay thin: parse the form, call [`crate::db`] or the run manager,
//! render a view. Business rules live in the layers below, so a rule cannot be
//! enforced in one route and forgotten in another.
//!
//! Successful mutations redirect (so a refresh does not resubmit) with a short
//! `?ok=` code that maps to a message; failed ones re-render the form with the
//! submitted values and an error banner, which is what lets an operator fix a
//! typo without retyping everything.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use maud::Markup;
use serde::Deserialize;
use tracing::warn;
use uuid::Uuid;

use crate::config::DeviceFilter;
use crate::db::{DeviceInput, SettingsInput, User};
use crate::error::Error;
use crate::web::runner::{self, TRIGGER_MANUAL};
use crate::web::session::{self, Operator};
use crate::web::views;
use crate::web::{AppState, Form};

/// How long a login session lasts, mirrored into the cookie.
const SESSION_DAYS: i64 = 30;

/// Commits are addressed by hex id; anything else is not ours to resolve.
fn is_hex(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

/// A handler failure that could not be shown inline on a form.
pub struct AppError(Error);

impl From<Error> for AppError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let message = crate::error::chain(&self.0);
        let (status, title) = match &self.0 {
            Error::NotFound(what) => (StatusCode::NOT_FOUND, format!("No such {what}")),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "Something broke".into()),
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            warn!("{message}");
        }
        (
            status,
            Html(views::error_page(None, &title, &message).into_string()),
        )
            .into_response()
    }
}

type Result<T = Response> = std::result::Result<T, AppError>;

fn page(markup: Markup) -> Response {
    Html(markup.into_string()).into_response()
}

/// Query string carrying a flash code between a POST and its redirect.
#[derive(Debug, Default, Deserialize)]
pub struct Flash {
    ok: Option<String>,
}

impl Flash {
    /// Map a short code to a message. Codes rather than free text so nothing
    /// user-supplied is ever reflected back into the page.
    fn message(&self) -> Option<&'static str> {
        match self.ok.as_deref() {
            Some("created") => Some("Device added."),
            Some("saved") => Some("Changes saved."),
            Some("deleted") => Some("Device deleted. Its history in Git is kept."),
            Some("settings") => Some("Settings saved."),
            Some("enabled") => Some("Device enabled."),
            Some("disabled") => Some("Device disabled."),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

pub async fn login_page(State(state): State<AppState>) -> Result {
    // A fresh deployment has no accounts: send the first visitor to setup
    // rather than to a login form nobody can pass.
    if state.db.user_count().await? == 0 {
        return Ok(Redirect::to("/setup").into_response());
    }
    Ok(page(views::login(None)))
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result {
    let client_ip = client_ip(&headers, peer);

    // Throttle before verifying, so a locked-out attacker costs one indexed
    // count rather than an Argon2 hash.
    if let Some(seconds) = state
        .db
        .login_lockout(&form.username, client_ip.as_deref())
        .await?
    {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Html(
                views::login(Some(&format!(
                    "Too many failed sign-in attempts. Try again in {}.",
                    humanize(seconds)
                )))
                .into_string(),
            ),
        )
            .into_response());
    }

    let authenticated = state
        .db
        .authenticate(&form.username, &form.password)
        .await?;
    state
        .db
        .record_login_attempt(
            &form.username,
            client_ip.as_deref(),
            authenticated.is_some(),
        )
        .await?;

    match authenticated {
        Some(user) => {
            let agent = headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok());
            let token = state.db.create_session(user.id, agent).await?;
            Ok((
                [(
                    header::SET_COOKIE,
                    session::set_cookie(&token, SESSION_DAYS),
                )],
                Redirect::to("/"),
            )
                .into_response())
        }
        // One message for both a wrong password and an unknown user, so the
        // form cannot be used to enumerate accounts.
        None => Ok((
            StatusCode::UNAUTHORIZED,
            Html(views::login(Some("Wrong username or password.")).into_string()),
        )
            .into_response()),
    }
}

/// Best-effort client address for throttling.
///
/// Behind a reverse proxy the socket address is the proxy itself, so the first
/// entry of `X-Forwarded-For` is used when present. That header is
/// attacker-controlled unless the proxy overwrites it, which is exactly why the
/// per-username limit — which no header can influence — is the real gate.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> Option<String> {
    if let Some(forwarded) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(forwarded.to_string());
    }
    Some(peer.ip().to_string())
}

/// "12 minutes", "45 seconds" — for a message an operator reads once.
fn humanize(seconds: i64) -> String {
    if seconds >= 120 {
        format!("{} minutes", (seconds + 59) / 60)
    } else if seconds >= 60 {
        "a minute".to_string()
    } else {
        format!("{seconds} seconds")
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result {
    if let Some(header) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
        && let Some(token) = session::cookie_value(header, session::COOKIE_NAME)
    {
        state.db.delete_session(&token).await?;
    }
    Ok((
        [(header::SET_COOKIE, session::clear_cookie())],
        Redirect::to("/login"),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm: String,
}

pub async fn setup_page(State(state): State<AppState>) -> Result {
    if state.db.user_count().await? > 0 {
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(page(views::setup(None)))
}

pub async fn setup(State(state): State<AppState>, Form(form): Form<SetupForm>) -> Result {
    // Guard against a second account being created through this open endpoint.
    if state.db.user_count().await? > 0 {
        return Ok(Redirect::to("/login").into_response());
    }
    if form.password != form.confirm {
        return Ok(page(views::setup(Some("The passwords do not match."))));
    }
    match state.db.create_user(&form.username, &form.password).await {
        Ok(user_id) => {
            let token = state.db.create_session(user_id, None).await?;
            Ok((
                [(
                    header::SET_COOKIE,
                    session::set_cookie(&token, SESSION_DAYS),
                )],
                Redirect::to("/"),
            )
                .into_response())
        }
        Err(error) => Ok(page(views::setup(Some(&crate::error::chain(&error))))),
    }
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

pub async fn dashboard(State(state): State<AppState>, Operator(user): Operator) -> Result {
    let devices = state.db.devices().await?;
    let runs = state.db.recent_runs(8).await?;
    let settings = state.db.settings().await?;
    let live = state.runs.latest();

    Ok(page(views::dashboard(
        &user,
        &devices,
        &runs,
        live.as_ref(),
        settings.remote_url.is_some(),
        &state.repo_path.display().to_string(),
    )))
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeviceForm {
    name: String,
    host: String,
    port: String,
    username: String,
    tenant: String,
    tags: String,
    auth_kind: String,
    secret: String,
    private_key_path: String,
    /// Absent when the checkbox is unticked.
    enabled: Option<String>,
}

impl DeviceForm {
    fn to_input(&self) -> DeviceInput {
        DeviceInput {
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port.trim().parse().unwrap_or(22),
            username: self.username.clone(),
            tenant: self.tenant.clone(),
            tags: self
                .tags
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect(),
            enabled: self.enabled.is_some(),
            auth_kind: self.auth_kind.clone(),
            // An empty field means "keep the stored secret".
            secret: Some(self.secret.clone()).filter(|s| !s.is_empty()),
            private_key_path: Some(self.private_key_path.clone()).filter(|p| !p.is_empty()),
        }
    }
}

pub async fn devices(
    State(state): State<AppState>,
    Operator(user): Operator,
    Query(flash): Query<Flash>,
) -> Result {
    let devices = state.db.devices().await?;
    Ok(page(views::devices(&user, &devices, flash.message())))
}

pub async fn new_device(State(state): State<AppState>, Operator(user): Operator) -> Result {
    let tenants = tenant_names(&state).await?;
    Ok(page(views::device_form(&user, None, None, None, &tenants)))
}

pub async fn create_device(
    State(state): State<AppState>,
    Operator(user): Operator,
    Form(form): Form<DeviceForm>,
) -> Result {
    match state.db.create_device(&form.to_input()).await {
        Ok(_) => Ok(Redirect::to("/devices?ok=created").into_response()),
        Err(error) => {
            // Re-render rather than redirect, so the operator keeps their input.
            let tenants = tenant_names(&state).await?;
            let preview = preview_row(&form, None);
            Ok(page(views::device_form(
                &user,
                Some(&preview),
                // No target: this is still a create, so the retry must post to
                // /devices rather than to an update of a nonexistent device.
                None,
                Some(&crate::error::chain(&error)),
                &tenants,
            )))
        }
    }
}

pub async fn edit_device(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    let device = state.db.device(id).await?;
    let tenants = tenant_names(&state).await?;
    Ok(page(views::device_form(
        &user,
        Some(&device),
        Some(device.id),
        None,
        &tenants,
    )))
}

pub async fn update_device(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path(id): Path<Uuid>,
    Form(form): Form<DeviceForm>,
) -> Result {
    match state.db.update_device(id, &form.to_input()).await {
        Ok(()) => Ok(Redirect::to("/devices?ok=saved").into_response()),
        Err(error) => {
            let existing = state.db.device(id).await?;
            let tenants = tenant_names(&state).await?;
            let preview = preview_row(&form, Some(&existing));
            Ok(page(views::device_form(
                &user,
                Some(&preview),
                Some(id),
                Some(&crate::error::chain(&error)),
                &tenants,
            )))
        }
    }
}

pub async fn delete_device(
    State(state): State<AppState>,
    Operator(_): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    state.db.delete_device(id).await?;
    Ok(Redirect::to("/devices?ok=deleted").into_response())
}

/// Connect to one device and report what it says, without exporting anything.
///
/// Renders the history page directly instead of redirecting: the result is the
/// point of the request, and a redirect would have nowhere to carry it.
pub async fn test_device(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    let device = state.db.device(id).await?;
    let config = state.db.runtime_config(state.repo_path.clone()).await?;

    let (ok, warning) = match crate::backup::test_device(&config, &device.name).await {
        Ok(info) => {
            state.db.record_probe(id, &info).await?;
            (Some(format!("Connected: {}", info.describe())), None)
        }
        Err(error) => (None, Some(crate::error::chain(&error))),
    };

    // Re-read so the page shows whatever the probe just learned.
    let device = state.db.device(id).await?;
    render_history(&state, &user, device, ok.as_deref(), warning.as_deref()).await
}

pub async fn device_history(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path(id): Path<Uuid>,
    Query(flash): Query<Flash>,
) -> Result {
    let device = state.db.device(id).await?;
    render_history(&state, &user, device, flash.message(), None).await
}

async fn render_history(
    state: &AppState,
    user: &User,
    device: crate::db::DeviceRow,
    flash: Option<&str>,
    warning: Option<&str>,
) -> Result {
    let settings = state.db.settings().await?;
    let relative = crate::db::backup_path_for(&device, &settings.path_template);
    let events = state.db.device_events(device.id, 10).await?;

    // The repository may not exist yet on a brand new deployment; an empty
    // history is the honest answer, not an error page.
    let history = match state.open_repo().await {
        Ok(repo) => repo.history(&relative, 50).unwrap_or_default(),
        Err(error) => {
            warn!("{}", crate::error::chain(&error));
            Vec::new()
        }
    };

    Ok(page(views::device_history(
        user,
        &device,
        &relative.display().to_string(),
        &history,
        &events,
        flash,
        warning,
    )))
}

pub async fn device_diff(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path((id, commit)): Path<(Uuid, String)>,
) -> Result {
    if !is_hex(&commit) {
        return Err(Error::NotFound("commit").into());
    }
    let device = state.db.device(id).await?;
    let settings = state.db.settings().await?;
    let relative = crate::db::backup_path_for(&device, &settings.path_template);

    let repo = state.open_repo().await?;
    let lines = repo.diff(None, &commit, &relative)?;
    let subject = repo
        .history(&relative, 200)
        .unwrap_or_default()
        .into_iter()
        .find(|entry| entry.id == commit)
        .map(|entry| entry.summary)
        .unwrap_or_default();

    Ok(page(views::device_diff(
        &user, &device, &commit, &subject, &lines,
    )))
}

/// Turn a form submission back into a row so the form can be re-rendered with
/// what the operator typed, rather than with what is stored.
fn preview_row(form: &DeviceForm, existing: Option<&crate::db::DeviceRow>) -> crate::db::DeviceRow {
    let input = form.to_input();
    crate::db::DeviceRow {
        id: existing.map(|row| row.id).unwrap_or_else(Uuid::nil),
        tenant_id: existing.map(|row| row.tenant_id).unwrap_or_else(Uuid::nil),
        tenant: input.tenant,
        name: input.name,
        host: input.host,
        port: input.port,
        username: input.username,
        auth_kind: input.auth_kind,
        has_secret: existing.is_some_and(|row| row.has_secret),
        private_key_path: input.private_key_path,
        tags: input.tags,
        enabled: input.enabled,
        identity: existing.and_then(|row| row.identity.clone()),
        firmware: existing.and_then(|row| row.firmware.clone()),
        model: existing.and_then(|row| row.model.clone()),
        serial: existing.and_then(|row| row.serial.clone()),
        last_seen_at: existing.and_then(|row| row.last_seen_at),
        last_outcome: existing.and_then(|row| row.last_outcome.clone()),
        last_detail: existing.and_then(|row| row.last_detail.clone()),
    }
}

async fn tenant_names(state: &AppState) -> std::result::Result<Vec<String>, AppError> {
    let mut names: Vec<String> = state
        .db
        .devices()
        .await?
        .into_iter()
        .map(|device| device.tenant)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct StartRunForm {
    /// Present when the dry-run button was used.
    dry_run: Option<String>,
}

pub async fn start_run(
    State(state): State<AppState>,
    Operator(_): Operator,
    Form(form): Form<StartRunForm>,
) -> Result {
    start(&state, DeviceFilter::default(), form.dry_run.is_some()).await
}

/// Back up a single device, ignoring whether it is enabled.
pub async fn start_device_run(
    State(state): State<AppState>,
    Operator(_): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    let device = state.db.device(id).await?;
    start(&state, DeviceFilter::named(device.name), false).await
}

async fn start(state: &AppState, filter: DeviceFilter, dry_run: bool) -> Result {
    match state
        .runs
        .start(
            state.db.clone(),
            state.repo_path.clone(),
            TRIGGER_MANUAL,
            filter,
            dry_run,
        )
        .await
    {
        Ok(run_id) => Ok(Redirect::to(&format!("/runs/{run_id}")).into_response()),
        // Already running: send the operator to the run that is in flight
        // rather than showing an error they can do nothing about.
        Err(error) => match state.runs.latest() {
            Some(live) if !live.finished => {
                Ok(Redirect::to(&format!("/runs/{}", live.id)).into_response())
            }
            _ => Err(error.into()),
        },
    }
}

pub async fn runs(State(state): State<AppState>, Operator(user): Operator) -> Result {
    let rows = state.db.recent_runs(100).await?;
    Ok(page(views::runs(&user, &rows)))
}

pub async fn run_detail(
    State(state): State<AppState>,
    Operator(user): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    let live = state.runs.snapshot(id);
    // A run started before the last restart exists only in the database.
    let row = state.db.run(id).await.ok();
    if live.is_none() && row.is_none() {
        return Err(Error::NotFound("run").into());
    }
    let events = state.db.run_events(id).await.unwrap_or_default();
    Ok(page(views::run_detail(
        &user,
        id,
        live.as_ref(),
        row.as_ref(),
        &events,
    )))
}

/// Progress endpoint the run page polls. JSON, no HTML.
pub async fn run_progress(
    State(state): State<AppState>,
    Operator(_): Operator,
    Path(id): Path<Uuid>,
) -> Result {
    if let Some(live) = state.runs.snapshot(id) {
        return Ok(Json(runner::progress_json(&live)).into_response());
    }
    // Not the run this process is tracking: report its stored state so the
    // page stops polling instead of hanging on "running".
    let row = state.db.run(id).await?;
    Ok(Json(serde_json::json!({
        "id": row.id,
        "finished": row.status != "running",
        "failed": row.status == "failed",
        "summary": format!(
            "{} changed, {} unchanged, {} failed",
            row.changed, row.unchanged, row.failed
        ),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    remote_url: String,
    remote_branch: String,
    git_username: String,
    git_token: String,
    remote_push: Option<String>,
    allow_invalid_certs: Option<String>,
    export_mode: String,
    host_key_policy: String,
    show_sensitive: Option<String>,
    schedule_enabled: Option<String>,
    schedule_hour: String,
    schedule_minute: String,
    concurrency: String,
    connect_timeout_secs: String,
    command_timeout_secs: String,
    path_template: String,
    committer_name: String,
    committer_email: String,
}

/// Numeric form fields arrive as text and may be empty or absurd; clamp rather
/// than reject, so a stray keystroke cannot 400 the whole settings page.
fn number(text: &str, default: i32, min: i32, max: i32) -> i32 {
    text.trim().parse().unwrap_or(default).clamp(min, max)
}

impl SettingsForm {
    fn to_input(&self) -> SettingsInput {
        SettingsInput {
            path_template: self.path_template.clone(),
            committer_name: self.committer_name.clone(),
            committer_email: self.committer_email.clone(),
            remote_url: Some(self.remote_url.clone()).filter(|url| !url.trim().is_empty()),
            remote_branch: if self.remote_branch.trim().is_empty() {
                crate::config::DEFAULT_BRANCH.to_string()
            } else {
                self.remote_branch.clone()
            },
            remote_push: self.remote_push.is_some(),
            git_username: if self.git_username.trim().is_empty() {
                "x-access-token".to_string()
            } else {
                self.git_username.clone()
            },
            // Empty keeps the stored token; a lone "-" clears it.
            git_token: match self.git_token.trim() {
                "" => None,
                "-" => Some(String::new()),
                token => Some(token.to_string()),
            },
            export_mode: self.export_mode.clone(),
            show_sensitive: self.show_sensitive.is_some(),
            concurrency: number(&self.concurrency, 8, 1, 256),
            connect_timeout_secs: number(&self.connect_timeout_secs, 10, 1, 600),
            command_timeout_secs: number(&self.command_timeout_secs, 120, 1, 3600),
            host_key_policy: self.host_key_policy.clone(),
            schedule_enabled: self.schedule_enabled.is_some(),
            schedule_hour: number(&self.schedule_hour, 2, 0, 23),
            schedule_minute: number(&self.schedule_minute, 30, 0, 59),
            allow_invalid_certs: self.allow_invalid_certs.is_some(),
        }
    }
}

pub async fn settings_page(
    State(state): State<AppState>,
    Operator(user): Operator,
    Query(flash): Query<Flash>,
) -> Result {
    let settings = state.db.settings().await?;
    Ok(page(views::settings(
        &user,
        &settings,
        &state.repo_path.display().to_string(),
        flash.message(),
        None,
    )))
}

pub async fn save_settings(
    State(state): State<AppState>,
    Operator(user): Operator,
    Form(form): Form<SettingsForm>,
) -> Result {
    match state.db.update_settings(&form.to_input()).await {
        Ok(()) => Ok(Redirect::to("/settings?ok=settings").into_response()),
        Err(error) => {
            let settings = state.db.settings().await?;
            Ok(page(views::settings(
                &user,
                &settings,
                &state.repo_path.display().to_string(),
                None,
                Some(&crate::error::chain(&error)),
            )))
        }
    }
}

/// Save the submitted settings, then check the remote with them.
///
/// This used to test *without* saving, and re-rendered the page from stored
/// settings — so an operator who typed a URL, pressed the button and then
/// pressed Save wrote an empty URL, because the field had silently reverted.
/// Saving first removes that trap: whatever is on screen is what was stored and
/// what was tested.
pub async fn test_remote(
    State(state): State<AppState>,
    Operator(user): Operator,
    Form(form): Form<SettingsForm>,
) -> Result {
    let repo_path = state.repo_path.display().to_string();

    if let Err(error) = state.db.update_settings(&form.to_input()).await {
        let stored = state.db.settings().await?;
        return Ok(page(views::settings(
            &user,
            &stored,
            &repo_path,
            None,
            Some(&crate::error::chain(&error)),
        )));
    }

    let stored = state.db.settings().await?;
    let Some(url) = stored.remote_url.clone() else {
        return Ok(page(views::settings(
            &user,
            &stored,
            &repo_path,
            Some("Settings saved. No repository URL to test."),
            None,
        )));
    };

    let token = state.db.git_token().await?;
    let outcome = state
        .probe_remote(
            &url,
            &stored.remote_branch,
            &stored.git_username,
            token.as_deref(),
            stored.allow_invalid_certs,
        )
        .await;

    let (flash, error) = match outcome {
        Ok(message) => (Some(format!("Settings saved. {message}")), None),
        Err(error) => (
            Some("Settings saved, but the connection test failed.".to_string()),
            Some(crate::error::chain(&error)),
        ),
    };
    Ok(page(views::settings(
        &user,
        &stored,
        &repo_path,
        flash.as_deref(),
        error.as_deref(),
    )))
}

/// Anything unrouted.
pub async fn not_found(operator: Option<Operator>) -> Response {
    let user = operator.map(|Operator(user)| user);
    (
        StatusCode::NOT_FOUND,
        Html(
            views::error_page(user.as_ref(), "Page not found", "That URL does not exist.")
                .into_string(),
        ),
    )
        .into_response()
}
