//! HTML rendering.
//!
//! Server-rendered with `maud`, which checks the markup at compile time. There
//! is deliberately no JavaScript build step and no framework: the only script in
//! the whole application is the dozen lines that poll a run's progress. Forms
//! post and the page re-renders, which is enough for an admin panel and leaves
//! nothing to go stale in a container image.
//!
//! Every user-facing string lives in this file, so translating the UI means
//! editing one module.

use chrono::{DateTime, Utc};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use uuid::Uuid;

use crate::db::{DeviceRow, EventRow, RunRow, Settings, User};
use crate::git::{DiffKind, DiffLine, HistoryEntry};
use crate::web::runner::Live;

const STYLE: &str = r#"
:root {
  --bg: #0f1117; --panel: #171a21; --panel-2: #1e222b; --line: #2a2f3a;
  --text: #e6e8ee; --muted: #98a0b3; --accent: #4c8dff; --accent-dim: #2f5fa8;
  --ok: #3fb950; --warn: #d29922; --bad: #f85149; --radius: 8px;
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
header.top { display: flex; align-items: center; gap: 24px; padding: 0 24px;
  background: var(--panel); border-bottom: 1px solid var(--line); height: 56px; }
header.top .brand { font-weight: 700; letter-spacing: .3px; }
header.top .brand span { color: var(--accent); }
header.top nav { display: flex; gap: 18px; flex: 1; }
header.top nav a { color: var(--muted); padding: 4px 0; border-bottom: 2px solid transparent; }
header.top nav a.on { color: var(--text); border-bottom-color: var(--accent); }
header.top .who { color: var(--muted); font-size: 13px; display: flex; align-items: center; gap: 12px; }
header.top .brand .version { color: var(--muted); font-weight: 400; font-size: 12px;
  margin-left: 6px; letter-spacing: 0; }
footer.foot { max-width: 1120px; margin: 0 auto; padding: 12px 24px 32px;
  color: var(--muted); font-size: 12px; text-align: center; }
main { max-width: 1120px; margin: 0 auto; padding: 24px; }
h1 { font-size: 22px; margin: 0 0 4px; }
h2 { font-size: 16px; margin: 28px 0 10px; }
p.sub { color: var(--muted); margin: 0 0 20px; }
.card { background: var(--panel); border: 1px solid var(--line);
  border-radius: var(--radius); padding: 18px; margin-bottom: 18px; }
.row { display: flex; gap: 18px; flex-wrap: wrap; }
.row > * { flex: 1; min-width: 240px; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th { text-align: left; color: var(--muted); font-weight: 600; font-size: 12px;
  text-transform: uppercase; letter-spacing: .5px; padding: 8px 10px;
  border-bottom: 1px solid var(--line); }
td { padding: 10px; border-bottom: 1px solid var(--line); vertical-align: middle; }
tr:last-child td { border-bottom: none; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }
.muted { color: var(--muted); }
.badge { display: inline-block; padding: 2px 8px; border-radius: 999px;
  font-size: 12px; font-weight: 600; border: 1px solid transparent; }
.badge.ok { color: var(--ok); border-color: #23502e; background: #102616; }
.badge.info { color: var(--accent); border-color: #23385e; background: #101a2b; }
.badge.warn { color: var(--warn); border-color: #4d3c14; background: #24200f; }
.badge.bad { color: var(--bad); border-color: #5c2321; background: #2a1413; }
.badge.off { color: var(--muted); border-color: var(--line); background: var(--panel-2); }
button, .btn { font: inherit; cursor: pointer; border-radius: 6px; padding: 8px 14px;
  border: 1px solid var(--line); background: var(--panel-2); color: var(--text); }
button:hover, .btn:hover { border-color: var(--accent-dim); text-decoration: none; }
button.primary { background: var(--accent); border-color: var(--accent); color: #08101f; font-weight: 600; }
button.danger { color: var(--bad); }
button:disabled { opacity: .5; cursor: not-allowed; }
.actions { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
form.inline { display: inline; }
label { display: block; margin-bottom: 14px; font-size: 13px; color: var(--muted); }
label > span.req { color: var(--bad); }
input[type=text], input[type=password], input[type=number], select {
  width: 100%; margin-top: 5px; padding: 9px 10px; border-radius: 6px;
  border: 1px solid var(--line); background: var(--bg); color: var(--text); font: inherit; }
input:focus, select:focus { outline: none; border-color: var(--accent-dim); }
label.check { display: flex; align-items: center; gap: 8px; color: var(--text); }
label.check input { width: auto; margin: 0; }
.hint { color: var(--muted); font-size: 12px; margin-top: 4px; }
.banner { border-radius: var(--radius); padding: 12px 14px; margin-bottom: 18px; font-size: 14px; }
.banner.err { background: #2a1413; border: 1px solid #5c2321; color: #ffb4ae; }
.banner.ok { background: #102616; border: 1px solid #23502e; color: #7ee08c; }
.banner.warn { background: #24200f; border: 1px solid #4d3c14; color: #f0cd6d; }
pre.log { background: #0b0d12; border: 1px solid var(--line); border-radius: var(--radius);
  padding: 14px; overflow-x: auto; max-height: 460px; overflow-y: auto;
  font-family: ui-monospace, Menlo, monospace; font-size: 13px; margin: 0; white-space: pre-wrap; }
pre.diff { background: #0b0d12; border: 1px solid var(--line); border-radius: var(--radius);
  padding: 0; overflow-x: auto; font-family: ui-monospace, Menlo, monospace;
  font-size: 13px; margin: 0; }
pre.diff .l { display: block; padding: 0 14px; white-space: pre; }
pre.diff .add { background: #0f2a16; color: #7ee08c; }
pre.diff .del { background: #2a1413; color: #ffb4ae; }
pre.diff .hunk { color: var(--accent); background: #101a2b; }
pre.diff .head { color: var(--muted); }
.center { max-width: 380px; margin: 8vh auto; }
.kv { display: grid; grid-template-columns: max-content 1fr; gap: 6px 18px; font-size: 14px; }
.kv dt { color: var(--muted); }
.kv dd { margin: 0; }
.stat { font-size: 26px; font-weight: 700; }
.empty { color: var(--muted); text-align: center; padding: 28px 0; }
"#;

/// Which nav item to highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Dashboard,
    Devices,
    Runs,
    Settings,
    None,
}

/// The page shell.
pub fn layout(title: &str, nav: Nav, operator: Option<&User>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "DonDude — " (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                @if let Some(user) = operator {
                    header.top {
                        div.brand {
                            "Don" span { "Dude" }
                            // Which build is deployed has to be answerable at a
                            // glance, not by shelling into the container.
                            span.version { "v" (crate::VERSION) }
                        }
                        nav {
                            a href="/" class=[on(nav == Nav::Dashboard)] { "Dashboard" }
                            a href="/devices" class=[on(nav == Nav::Devices)] { "Devices" }
                            a href="/runs" class=[on(nav == Nav::Runs)] { "Runs" }
                            a href="/settings" class=[on(nav == Nav::Settings)] { "Settings" }
                        }
                        div.who {
                            span { (user.username) }
                            form.inline method="post" action="/logout" {
                                button { "Sign out" }
                            }
                        }
                    }
                }
                main { (body) }
                footer.foot {
                    "DonDude v" (crate::VERSION)
                    " · "
                    a href="https://github.com/rimpianto/mikrotik.DonDude" { "source" }
                }
            }
        }
    }
}

fn on(active: bool) -> Option<&'static str> {
    active.then_some("on")
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

pub fn login(error: Option<&str>) -> Markup {
    layout(
        "Sign in",
        Nav::None,
        None,
        html! {
            div.center {
                h1 { "Don" span style="color:var(--accent)" { "Dude" } }
                p.sub { "RouterOS fleet backups" }
                @if let Some(message) = error {
                    div.banner.err { (message) }
                }
                div.card {
                    form method="post" action="/login" {
                        label {
                            "Username"
                            input type="text" name="username" autocomplete="username"
                                autofocus required;
                        }
                        label {
                            "Password"
                            input type="password" name="password"
                                autocomplete="current-password" required;
                        }
                        button.primary type="submit" style="width:100%" { "Sign in" }
                    }
                }
            }
        },
    )
}

/// First-run screen: no accounts exist yet.
pub fn setup(error: Option<&str>) -> Markup {
    layout(
        "Set up",
        Nav::None,
        None,
        html! {
            div.center {
                h1 { "Welcome to Don" span style="color:var(--accent)" { "Dude" } }
                p.sub { "Create the administrator account to get started." }
                @if let Some(message) = error {
                    div.banner.err { (message) }
                }
                div.card {
                    form method="post" action="/setup" {
                        label {
                            "Username"
                            input type="text" name="username" value="admin"
                                autocomplete="username" autofocus required;
                        }
                        label {
                            "Password"
                            input type="password" name="password"
                                autocomplete="new-password" minlength="8" required;
                            div.hint { "At least 8 characters." }
                        }
                        label {
                            "Repeat password"
                            input type="password" name="confirm"
                                autocomplete="new-password" minlength="8" required;
                        }
                        button.primary type="submit" style="width:100%" { "Create account" }
                    }
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

pub fn dashboard(
    user: &User,
    devices: &[DeviceRow],
    runs: &[RunRow],
    live: Option<&Live>,
    remote_configured: bool,
    repo_path: &str,
) -> Markup {
    let enabled = devices.iter().filter(|device| device.enabled).count();
    let failing = devices
        .iter()
        .filter(|device| device.last_outcome.as_deref() == Some("failed"))
        .count();

    layout(
        "Dashboard",
        Nav::Dashboard,
        Some(user),
        html! {
            h1 { "Dashboard" }
            p.sub { "Backups are stored in " span.mono { (repo_path) } }

            @if devices.is_empty() {
                div.banner.warn {
                    "No devices yet. "
                    a href="/devices/new" { "Add your first router" }
                    " to start backing up."
                }
            }
            @if !remote_configured {
                div.banner.warn {
                    "No GitHub repository configured — backups are committed locally only. "
                    a href="/settings" { "Set up the remote" } "."
                }
            }

            div.row {
                div.card { div.muted { "Devices" } div.stat { (devices.len()) }
                    div.muted { (enabled) " enabled" } }
                div.card { div.muted { "Failing" }
                    div.stat style=[failing_style(failing)] { (failing) }
                    div.muted { "at last run" } }
                div.card { div.muted { "Runs recorded" } div.stat { (runs.len()) }
                    div.muted { "most recent first" } }
            }

            div.card {
                div.actions {
                    form method="post" action="/runs" {
                        button.primary type="submit" disabled[live.is_some_and(|l| !l.finished)] {
                            "Back up all devices now"
                        }
                    }
                    form method="post" action="/runs" {
                        input type="hidden" name="dry_run" value="1";
                        button type="submit" disabled[live.is_some_and(|l| !l.finished)] {
                            "Dry run"
                        }
                    }
                    @if let Some(live) = live {
                        @if !live.finished {
                            span.badge.info { "run in progress" }
                            a href={ "/runs/" (live.id) } { "Follow it" }
                        }
                    }
                }
                div.hint { "A dry run connects to every device and reports what would change, without writing or pushing anything." }
            }

            h2 { "Devices" }
            div.card {
                @if devices.is_empty() {
                    div.empty { "Nothing here yet." }
                } @else {
                    table {
                        thead { tr {
                            th { "Device" } th { "Address" } th { "Tenant" }
                            th { "Firmware" } th { "Last result" } th { "Last seen" }
                        } }
                        tbody { @for device in devices {
                            tr {
                                td {
                                    a href={ "/devices/" (device.id) "/history" } { (device.name) }
                                    @if !device.enabled { " " span.badge.off { "disabled" } }
                                }
                                td.mono { (device.addr()) }
                                td { (device.tenant) }
                                td.mono { (option(device.firmware.as_deref())) }
                                td { (outcome_badge(device.last_outcome.as_deref())) }
                                td.muted { (option_time(device.last_seen_at)) }
                            }
                        } }
                    }
                }
            }

            h2 { "Recent runs" }
            div.card { (runs_table(runs)) }
        },
    )
}

fn failing_style(failing: usize) -> Option<&'static str> {
    (failing > 0).then_some("color:var(--bad)")
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

pub fn devices(user: &User, devices: &[DeviceRow], flash: Option<&str>) -> Markup {
    layout(
        "Devices",
        Nav::Devices,
        Some(user),
        html! {
            div style="display:flex;justify-content:space-between;align-items:center" {
                div { h1 { "Devices" } p.sub { "The fleet DonDude backs up." } }
                a.btn href="/devices/new" { "Add device" }
            }
            @if let Some(message) = flash { div.banner.ok { (message) } }

            div.card {
                @if devices.is_empty() {
                    div.empty { "No devices yet. Add one to get started." }
                } @else {
                    table {
                        thead { tr {
                            th { "Device" } th { "Address" } th { "User" } th { "Auth" }
                            th { "Tenant" } th { "Tags" } th { "State" } th {}
                        } }
                        tbody { @for device in devices {
                            tr {
                                td { a href={ "/devices/" (device.id) "/history" } { (device.name) } }
                                td.mono { (device.addr()) }
                                td.mono { (device.username) }
                                td { (device.auth_kind) }
                                td { (device.tenant) }
                                td.muted { (device.tags.join(", ")) }
                                td {
                                    @if device.enabled { span.badge.ok { "enabled" } }
                                    @else { span.badge.off { "disabled" } }
                                }
                                td {
                                    div.actions {
                                        a.btn href={ "/devices/" (device.id) "/edit" } { "Edit" }
                                        form.inline method="post"
                                            action={ "/devices/" (device.id) "/test" } {
                                            button type="submit" { "Test" }
                                        }
                                        form.inline method="post"
                                            action={ "/devices/" (device.id) "/backup" } {
                                            button type="submit" { "Back up" }
                                        }
                                    }
                                }
                            }
                        } }
                    }
                }
            }
        },
    )
}

/// Add or edit a device.
///
/// `values` prefills the fields and `target` decides where the form posts.
/// They are separate because a *failed create* must re-render with what the
/// operator typed while still posting to the create endpoint — deriving the
/// action from the prefill would send them to an update of a device that does
/// not exist.
pub fn device_form(
    user: &User,
    values: Option<&DeviceRow>,
    target: Option<Uuid>,
    error: Option<&str>,
    known_tenants: &[String],
) -> Markup {
    let editing = target.is_some();
    let action = match target {
        Some(id) => format!("/devices/{id}"),
        None => "/devices".to_string(),
    };
    let device = values;
    let kind = device.map(|d| d.auth_kind.as_str()).unwrap_or("password");

    layout(
        if editing { "Edit device" } else { "Add device" },
        Nav::Devices,
        Some(user),
        html! {
            h1 { @if editing { "Edit device" } @else { "Add device" } }
            p.sub { "Credentials are encrypted before they are stored." }
            @if let Some(message) = error { div.banner.err { (message) } }

            div.card {
                form method="post" action=(action) {
                    div.row {
                        div {
                            label {
                                "Name " span.req { "*" }
                                input type="text" name="name" required
                                    value=(device.map(|d| d.name.clone()).unwrap_or_default());
                                div.hint { "Also the file name in the backup repository." }
                            }
                            label {
                                "Host or IP " span.req { "*" }
                                input type="text" name="host" required
                                    value=(device.map(|d| d.host.clone()).unwrap_or_default());
                            }
                            label {
                                "SSH port"
                                input type="number" name="port" min="1" max="65535"
                                    value=(device.map(|d| d.port).unwrap_or(22));
                            }
                        }
                        div {
                            label {
                                "SSH username " span.req { "*" }
                                input type="text" name="username" required
                                    value=(device.map(|d| d.username.clone()).unwrap_or_default());
                                div.hint { "A read-only RouterOS user is enough." }
                            }
                            label {
                                "Tenant"
                                input type="text" name="tenant" list="tenants"
                                    value=(device.map(|d| d.tenant.clone())
                                        .unwrap_or_else(|| "default".to_string()));
                                datalist #tenants {
                                    @for tenant in known_tenants { option value=(tenant) {} }
                                }
                                div.hint { "Groups devices into folders in the repository." }
                            }
                            label {
                                "Tags"
                                input type="text" name="tags"
                                    value=(device.map(|d| d.tags.join(", ")).unwrap_or_default());
                                div.hint { "Comma separated. Used to back up part of the fleet." }
                            }
                        }
                    }

                    h2 { "Authentication" }
                    label {
                        "Method"
                        select name="auth_kind" {
                            option value="password" selected[kind == "password"] { "Password" }
                            option value="key" selected[kind == "key"] { "SSH private key" }
                            option value="agent" selected[kind == "agent"] { "ssh-agent" }
                        }
                    }
                    label {
                        "Password or key passphrase"
                        input type="password" name="secret" autocomplete="new-password"
                            placeholder=(if editing && device.is_some_and(|d| d.has_secret) {
                                "unchanged — type to replace"
                            } else { "" });
                        div.hint {
                            "Leave empty to keep the stored one. Not needed for ssh-agent."
                        }
                    }
                    label {
                        "Private key path (inside the container)"
                        input type="text" name="private_key_path"
                            placeholder="/keys/id_ed25519"
                            value=(device.and_then(|d| d.private_key_path.clone())
                                .unwrap_or_default());
                        div.hint { "Only for the SSH private key method. Mount the key as a volume." }
                    }
                    label.check {
                        input type="checkbox" name="enabled" value="1"
                            checked[device.map(|d| d.enabled).unwrap_or(true)];
                        "Include in fleet-wide runs"
                    }

                    div.actions {
                        button.primary type="submit" {
                            @if editing { "Save changes" } @else { "Add device" }
                        }
                        a.btn href="/devices" { "Cancel" }
                        @if let Some(id) = target {
                            // `formaction` rather than a nested <form>: nesting
                            // forms is invalid HTML, and a browser that drops
                            // the inner one would make this button save instead
                            // of delete.
                            button.danger type="submit" formnovalidate
                                formaction={ "/devices/" (id) "/delete" }
                                onclick="return confirm('Delete this device? Its backup history in Git is kept.')" {
                                "Delete"
                            }
                        }
                    }
                }
            }
        },
    )
}

/// A device's commit history, plus its recent run outcomes.
pub fn device_history(
    user: &User,
    device: &DeviceRow,
    path: &str,
    history: &[HistoryEntry],
    events: &[EventRow],
    flash: Option<&str>,
    warning: Option<&str>,
) -> Markup {
    layout(
        &device.name,
        Nav::Devices,
        Some(user),
        html! {
            div style="display:flex;justify-content:space-between;align-items:center" {
                div {
                    h1 { (device.name) }
                    p.sub { span.mono { (device.addr()) } " · " (device.tenant) }
                }
                div.actions {
                    form.inline method="post" action={ "/devices/" (device.id) "/backup" } {
                        button.primary type="submit" { "Back up now" }
                    }
                    form.inline method="post" action={ "/devices/" (device.id) "/test" } {
                        button type="submit" { "Test connection" }
                    }
                    a.btn href={ "/devices/" (device.id) "/edit" } { "Edit" }
                }
            }
            @if let Some(message) = flash { div.banner.ok { (message) } }
            @if let Some(message) = warning { div.banner.warn { (message) } }

            div.row {
                div.card {
                    dl.kv {
                        dt { "Identity" } dd.mono { (option(device.identity.as_deref())) }
                        dt { "Model" } dd.mono { (option(device.model.as_deref())) }
                        dt { "Firmware" } dd.mono { (option(device.firmware.as_deref())) }
                        dt { "Serial" } dd.mono { (option(device.serial.as_deref())) }
                        dt { "Last seen" } dd { (option_time(device.last_seen_at)) }
                        dt { "File" } dd.mono { (path) }
                    }
                }
                div.card {
                    div.muted { "Last result" }
                    div style="margin:8px 0" { (outcome_badge(device.last_outcome.as_deref())) }
                    div.muted.mono { (option(device.last_detail.as_deref())) }
                }
            }

            h2 { "Configuration history" }
            div.card {
                @if history.is_empty() {
                    div.empty { "No commits yet for this device." }
                } @else {
                    table {
                        thead { tr {
                            th { "When" } th { "Change" } th { "Lines" } th { "Commit" } th {}
                        } }
                        tbody { @for entry in history {
                            tr {
                                td.muted { (time(entry.when)) }
                                td { (entry.summary) }
                                td.mono {
                                    span style="color:var(--ok)" { "+" (entry.insertions) }
                                    " "
                                    span style="color:var(--bad)" { "−" (entry.deletions) }
                                }
                                td.mono.muted { (entry.short_id()) }
                                td {
                                    a.btn href={ "/devices/" (device.id) "/diff/" (entry.id) } {
                                        "View diff"
                                    }
                                }
                            }
                        } }
                    }
                }
            }

            h2 { "Recent runs" }
            div.card {
                @if events.is_empty() {
                    div.empty { "This device has not been included in a run yet." }
                } @else {
                    table {
                        thead { tr { th { "When" } th { "Result" } th { "Detail" } th { "Took" } } }
                        tbody { @for event in events {
                            tr {
                                td.muted { (time(event.created_at)) }
                                td { (outcome_badge(Some(&event.outcome))) }
                                td.mono.muted { (option(event.detail.as_deref())) }
                                td.muted { (event.elapsed_ms) " ms" }
                            }
                        } }
                    }
                }
            }
        },
    )
}

pub fn device_diff(
    user: &User,
    device: &DeviceRow,
    commit: &str,
    subject: &str,
    lines: &[DiffLine],
) -> Markup {
    layout(
        "Diff",
        Nav::Devices,
        Some(user),
        html! {
            h1 { (device.name) }
            p.sub {
                a href={ "/devices/" (device.id) "/history" } { "← history" }
                " · " span.mono { (&commit[..commit.len().min(8)]) } " · " (subject)
            }
            div.card style="padding:0;overflow:hidden" {
                @if lines.is_empty() {
                    div.empty { "This commit did not change this device's file." }
                } @else {
                    pre.diff { @for line in lines {
                        span class=(diff_class(line.kind)) { (line.text) "\n" }
                    } }
                }
            }
        },
    )
}

fn diff_class(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Added => "l add",
        DiffKind::Removed => "l del",
        DiffKind::Hunk => "l hunk",
        DiffKind::Header => "l head",
        DiffKind::Context => "l",
    }
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

pub fn runs(user: &User, rows: &[RunRow]) -> Markup {
    layout(
        "Runs",
        Nav::Runs,
        Some(user),
        html! {
            h1 { "Runs" }
            p.sub { "Every backup run, newest first." }
            div.card { (runs_table(rows)) }
        },
    )
}

fn runs_table(rows: &[RunRow]) -> Markup {
    html! {
        @if rows.is_empty() {
            div.empty { "No runs yet." }
        } @else {
            table {
                thead { tr {
                    th { "Started" } th { "Trigger" } th { "Status" }
                    th { "Changed" } th { "Unchanged" } th { "Failed" } th { "Push" } th {}
                } }
                tbody { @for row in rows {
                    tr {
                        td.muted { (time(row.started_at)) }
                        td { (row.trigger) }
                        td { (status_badge(&row.status)) }
                        td { (row.changed) }
                        td.muted { (row.unchanged) }
                        td { @if row.failed > 0 {
                                span style="color:var(--bad)" { (row.failed) }
                            } @else { "0" } }
                        td.muted {
                            @if row.pushed { "pushed" }
                            @else { (option(row.push_detail.as_deref())) }
                        }
                        td { a.btn href={ "/runs/" (row.id) } { "Details" } }
                    }
                } }
            }
        }
    }
}

/// One run. While it is in flight the log is polled; once finished the page
/// shows the per-device table from the database.
pub fn run_detail(
    user: &User,
    run_id: Uuid,
    live: Option<&Live>,
    row: Option<&RunRow>,
    events: &[EventRow],
) -> Markup {
    let in_flight = live.is_some_and(|live| !live.finished);
    let log = live
        .map(|live| live.log.join("\n"))
        .or_else(|| row.map(|row| row.log.clone()))
        .unwrap_or_default();
    let summary = live
        .and_then(|live| live.summary.clone())
        .or_else(|| row.map(run_summary));

    layout(
        "Run",
        Nav::Runs,
        Some(user),
        html! {
            h1 { "Backup run" }
            p.sub {
                span.mono { (run_id) }
                @if let Some(row) = row { " · started " (time(row.started_at)) }
            }

            div.card {
                div.actions {
                    @if in_flight {
                        span.badge.info #status { "running…" }
                    } @else if let Some(row) = row {
                        span #status { (status_badge(&row.status)) }
                    }
                    @if let Some(summary) = &summary { span.muted #summary { (summary) } }
                }
            }

            h2 { "Log" }
            pre.log #log { (log) }

            @if !events.is_empty() {
                h2 { "Devices" }
                div.card {
                    table {
                        thead { tr {
                            th { "Device" } th { "Result" } th { "Lines" }
                            th { "Detail" } th { "Took" }
                        } }
                        tbody { @for event in events {
                            tr {
                                td { (event.device_name) }
                                td { (outcome_badge(Some(&event.outcome))) }
                                td.mono {
                                    @if event.insertions > 0 || event.deletions > 0 {
                                        span style="color:var(--ok)" { "+" (event.insertions) }
                                        " "
                                        span style="color:var(--bad)" { "−" (event.deletions) }
                                    } @else { span.muted { "—" } }
                                }
                                td.mono.muted { (option(event.detail.as_deref())) }
                                td.muted { (event.elapsed_ms) " ms" }
                            }
                        } }
                    }
                }
            }

            @if in_flight {
                script { (PreEscaped(POLL_SCRIPT)) }
            }
        },
    )
}

/// Poll the progress endpoint while a run is in flight, then reload once so the
/// finished page renders from the database like any other run.
const POLL_SCRIPT: &str = r#"
(function () {
  const id = location.pathname.split('/').pop();
  const log = document.getElementById('log');
  async function tick() {
    try {
      const response = await fetch('/api/runs/' + id, { cache: 'no-store' });
      if (!response.ok) { setTimeout(tick, 3000); return; }
      const state = await response.json();
      if (Array.isArray(state.log)) {
        log.textContent = state.log.join('\n');
        log.scrollTop = log.scrollHeight;
      }
      if (state.finished) { location.reload(); return; }
    } catch (error) { /* transient: keep polling */ }
    setTimeout(tick, 1000);
  }
  tick();
})();
"#;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub fn settings(
    user: &User,
    settings: &Settings,
    repo_path: &str,
    flash: Option<&str>,
    error: Option<&str>,
) -> Markup {
    layout(
        "Settings",
        Nav::Settings,
        Some(user),
        html! {
            h1 { "Settings" }
            p.sub { "Where backups go, how they are captured, and when." }
            @if let Some(message) = flash { div.banner.ok { (message) } }
            @if let Some(message) = error { div.banner.err { (message) } }

            form method="post" action="/settings" {
                h2 { "GitHub repository" }
                div.card {
                    label {
                        "Repository URL"
                        input type="text" name="remote_url"
                            placeholder="https://github.com/you/mikrotik-backups.git"
                            value=(settings.remote_url.clone().unwrap_or_default());
                        div.hint {
                            "Create an empty "
                            strong { "private" }
                            " repository on GitHub and paste its HTTPS URL. \
                             Leave empty to keep backups on this machine only."
                        }
                        div.hint {
                            "Set this up "
                            strong { "before the first backup" }
                            " if you can. Committing locally first and adding the \
                             repository afterwards leaves two histories that cannot be \
                             merged automatically, which then needs a git command to \
                             untangle."
                        }
                    }
                    div.row {
                        label {
                            "Branch"
                            input type="text" name="remote_branch"
                                value=(settings.remote_branch.clone());
                        }
                        label {
                            "Username"
                            input type="text" name="git_username"
                                value=(settings.git_username.clone());
                            div.hint { "Ignored by GitHub when the password is a token." }
                        }
                    }
                    label {
                        "Access token"
                        input type="password" name="git_token" autocomplete="off"
                            placeholder=(if settings.has_git_token {
                                "stored — type to replace"
                            } else {
                                "github_pat_..."
                            });
                        div.hint {
                            @if settings.has_git_token {
                                "A token is stored (encrypted). Leave empty to keep it, or type "
                                span.mono { "-" }
                                " to remove it. "
                            }
                            "Use a fine-grained token limited to that one repository with "
                            strong { "Contents: Read and write" } "."
                        }
                    }
                    label.check {
                        input type="checkbox" name="remote_push" value="1"
                            checked[settings.remote_push];
                        "Push to GitHub after each run"
                    }
                    div.actions {
                        button type="submit" formaction="/settings/test" formnovalidate {
                            "Save and test connection"
                        }
                        span.hint {
                            "Stores these settings, then connects to check the URL and token."
                        }
                    }
                }

                h2 { "Capture" }
                div.card {
                    div.row {
                        label {
                            "Export detail"
                            select name="export_mode" {
                                @for (value, text) in [
                                    ("terse", "terse — one command per line (recommended)"),
                                    ("compact", "compact — only non-default settings"),
                                    ("verbose", "verbose — every property"),
                                ] {
                                    option value=(value) selected[settings.export_mode == value] {
                                        (text)
                                    }
                                }
                            }
                        }
                        label {
                            "Host key policy"
                            select name="host_key_policy" {
                                @for (value, text) in [
                                    ("accept-new", "accept-new — trust on first use (recommended)"),
                                    ("strict", "strict — key must already be known"),
                                    ("off", "off — no verification (lab only)"),
                                ] {
                                    option value=(value)
                                        selected[settings.host_key_policy == value] { (text) }
                                }
                            }
                        }
                    }
                    label.check {
                        input type="checkbox" name="show_sensitive" value="1"
                            checked[settings.show_sensitive];
                        "Include secrets in exports (PSKs, PPP passwords, SNMP communities)"
                    }
                    div.hint {
                        "Off by default: those values would be committed to Git in clear text, \
                         and the repository is a softer target than the routers themselves."
                    }
                }

                h2 { "Schedule" }
                div.card {
                    label.check {
                        input type="checkbox" name="schedule_enabled" value="1"
                            checked[settings.schedule_enabled];
                        "Back up automatically every day"
                    }
                    div.row {
                        label {
                            "Hour (UTC)"
                            input type="number" name="schedule_hour" min="0" max="23"
                                value=(settings.schedule_hour);
                        }
                        label {
                            "Minute"
                            input type="number" name="schedule_minute" min="0" max="59"
                                value=(settings.schedule_minute);
                        }
                    }
                    div.hint { "Times are UTC, so they do not shift with daylight saving." }
                }

                h2 { "Advanced" }
                div.card {
                    div.row {
                        label {
                            "Parallel devices"
                            input type="number" name="concurrency" min="1" max="256"
                                value=(settings.concurrency);
                        }
                        label {
                            "Connect timeout (s)"
                            input type="number" name="connect_timeout_secs" min="1" max="600"
                                value=(settings.connect_timeout_secs);
                        }
                        label {
                            "Command timeout (s)"
                            input type="number" name="command_timeout_secs" min="1" max="3600"
                                value=(settings.command_timeout_secs);
                        }
                    }
                    div.row {
                        label {
                            "File layout"
                            input type="text" name="path_template"
                                value=(settings.path_template.clone());
                            div.hint { "Placeholders: {tenant} {device} {host}" }
                        }
                        label {
                            "Commit author name"
                            input type="text" name="committer_name"
                                value=(settings.committer_name.clone());
                        }
                        label {
                            "Commit author email"
                            input type="text" name="committer_email"
                                value=(settings.committer_email.clone());
                        }
                    }
                    dl.kv {
                        dt { "Repository path" } dd.mono { (repo_path) }
                    }
                    div.hint {
                        "Set by the deployment (a mounted volume), not editable here."
                    }
                }

                div.actions { button.primary type="submit" { "Save settings" } }
            }
        },
    )
}

/// Standalone page for an unexpected failure.
pub fn error_page(user: Option<&User>, title: &str, message: &str) -> Markup {
    layout(
        title,
        Nav::None,
        user,
        html! {
            h1 { (title) }
            div.banner.err { (message) }
            a.btn href="/" { "Back to the dashboard" }
        },
    )
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn option(value: Option<&str>) -> Markup {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => html! { (text) },
        None => html! { span.muted { "—" } },
    }
}

fn time(when: DateTime<Utc>) -> String {
    when.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn option_time(when: Option<DateTime<Utc>>) -> Markup {
    match when {
        Some(when) => html! { (time(when)) },
        None => html! { span.muted { "never" } },
    }
}

fn outcome_badge(outcome: Option<&str>) -> Markup {
    match outcome {
        Some("committed") => html! { span.badge.ok { "committed" } },
        Some("unchanged") => html! { span.badge.info { "unchanged" } },
        Some("would_change") => html! { span.badge.warn { "would change" } },
        Some("failed") => html! { span.badge.bad { "failed" } },
        _ => html! { span.badge.off { "never run" } },
    }
}

fn status_badge(status: &str) -> Markup {
    match status {
        "completed" => html! { span.badge.ok { "completed" } },
        "running" => html! { span.badge.info { "running" } },
        _ => html! { span.badge.bad { "failed" } },
    }
}

fn run_summary(row: &RunRow) -> String {
    format!(
        "{} changed, {} unchanged, {} failed",
        row.changed, row.unchanged, row.failed
    )
}
