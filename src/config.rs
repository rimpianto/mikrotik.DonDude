//! Runtime configuration: the shape the engine runs on.
//!
//! There is no configuration file. These structures are assembled from the
//! database by [`crate::db`] — with credentials already decrypted — and handed
//! to the backup pipeline. Keeping them as plain Rust types means the engine
//! (`routeros`, `git`, `backup`) never touches SQL, and tests can build a fleet
//! in three lines without a database.
//!
//! Secrets live *in* these types, so [`DeviceAuth`] and [`GitAuth`] carry
//! hand-written `Debug` implementations that redact them. Do not derive `Debug`
//! on either.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use uuid::Uuid;

use crate::error::{Error, Result};

/// Everything one backup run needs.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub backup: Backup,
    pub export: Export,
    pub devices: Vec<Device>,
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct General {
    /// How many devices to talk to at once.
    pub concurrency: usize,
    pub connect_timeout_secs: u64,
    /// Per-command budget. Covers a whole `/export`.
    pub command_timeout_secs: u64,
    pub host_key_policy: HostKeyPolicy,
    /// `known_hosts` file; defaults to `~/.ssh/known_hosts`.
    pub known_hosts: Option<PathBuf>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            concurrency: 8,
            connect_timeout_secs: 10,
            command_timeout_secs: 120,
            host_key_policy: HostKeyPolicy::default(),
            known_hosts: None,
        }
    }
}

impl General {
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }

    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_secs)
    }

    /// Resolved `known_hosts` path, defaulting to `~/.ssh/known_hosts`.
    pub fn known_hosts_path(&self) -> Option<PathBuf> {
        match &self.known_hosts {
            Some(path) => Some(expand_tilde(path)),
            None => std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".ssh").join("known_hosts")),
        }
    }
}

/// Host-key trust policy for device connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// The key must already be in `known_hosts` and match.
    Strict,
    /// Trust and record a key on first sight; refuse if it later changes.
    /// Matches OpenSSH's `StrictHostKeyChecking=accept-new`.
    #[default]
    AcceptNew,
    /// No verification at all. Open to man-in-the-middle; lab use only.
    Off,
}

impl HostKeyPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::AcceptNew => "accept-new",
            Self::Off => "off",
        }
    }

    /// Parse the stored value. Unknown text falls back to the safe default
    /// rather than to `off`.
    pub fn parse(text: &str) -> Self {
        match text {
            "strict" => Self::Strict,
            "off" => Self::Off,
            _ => Self::AcceptNew,
        }
    }
}

// ---------------------------------------------------------------------------
// Backup repository
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Backup {
    /// Working tree of the backup repository. A deployment concern (a mounted
    /// volume), not something an operator edits in the browser.
    pub repo_path: PathBuf,
    /// Where each device's export lands. Placeholders: `{tenant}`, `{device}`,
    /// `{host}`.
    pub path_template: String,
    pub committer: Committer,
    pub remote: Option<Remote>,
}

impl Backup {
    pub fn branch(&self) -> &str {
        self.remote
            .as_ref()
            .map(|remote| remote.branch.as_str())
            .unwrap_or(DEFAULT_BRANCH)
    }
}

pub const DEFAULT_BRANCH: &str = "main";
pub const DEFAULT_PATH_TEMPLATE: &str = "{tenant}/{device}.rsc";

#[derive(Debug, Clone)]
pub struct Committer {
    pub name: String,
    pub email: String,
}

impl Default for Committer {
    fn default() -> Self {
        Self {
            name: "DonDude".to_string(),
            email: "dondude@localhost".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub branch: String,
    /// Push after committing. `false` keeps history local.
    pub push: bool,
    pub auth: GitAuth,
    /// Skip TLS certificate verification for this remote.
    ///
    /// For a self-hosted Gitea, Forgejo or GitLab with a self-signed
    /// certificate. It removes the protection against a man-in-the-middle on
    /// the push, so it stays off unless an operator asks for it; adding the
    /// instance's CA to the trust store is the better fix.
    pub allow_invalid_certs: bool,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("branch", &self.branch)
            .field("push", &self.push)
            .field("auth", &self.auth)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .finish()
    }
}

/// How to authenticate against the backup remote.
///
/// HTTP basic with a token only, deliberately: DonDude runs in a container, and
/// a deploy key would mean mounting a private key and exposing file paths in the
/// UI for no gain over a scoped token. This is what GitHub, Gitea, Forgejo and
/// GitLab all accept — GitHub ignores the username, the others check it.
#[derive(Clone)]
pub enum GitAuth {
    Token {
        username: String,
        token: String,
    },
    /// No credentials — a local path or an already-authenticated helper.
    None,
}

impl GitAuth {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Token { .. } => "https token",
            Self::None => "none",
        }
    }
}

// Tokens must never reach a log line or a panic message.
impl std::fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.describe())
    }
}

// ---------------------------------------------------------------------------
// Export behaviour
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportMode {
    /// `/export` — only values that differ from RouterOS defaults.
    Compact,
    /// `/export terse` — one command per line. Default: wrapped multi-line
    /// output turns a one-setting change into a large diff.
    #[default]
    Terse,
    /// `/export verbose` — every property, including defaults.
    Verbose,
}

impl ExportMode {
    fn as_arg(self) -> Option<&'static str> {
        match self {
            Self::Compact => None,
            Self::Terse => Some("terse"),
            Self::Verbose => Some("verbose"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Terse => "terse",
            Self::Verbose => "verbose",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "compact" => Self::Compact,
            "verbose" => Self::Verbose,
            _ => Self::Terse,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Export {
    pub mode: ExportMode,
    /// Include secrets (PSKs, PPP passwords, SNMP communities) in the export.
    /// Off by default: the output is committed to Git, and a backup repository
    /// is a far softer target than the routers it describes.
    pub show_sensitive: bool,
    /// Rewrite the volatile `# <date> by RouterOS <ver>` banner to a stable
    /// form. Without this, every run produces a diff.
    pub normalize_header: bool,
}

impl Default for Export {
    fn default() -> Self {
        Self {
            mode: ExportMode::default(),
            show_sensitive: false,
            normalize_header: true,
        }
    }
}

impl Export {
    /// The RouterOS command line for this export.
    pub fn command_line(&self) -> String {
        let mut parts = vec!["/export".to_string()];
        if let Some(arg) = self.mode.as_arg() {
            parts.push(arg.to_string());
        }
        if self.show_sensitive {
            parts.push("show-sensitive".to_string());
        }
        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Device {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Unique, stable identity. Also the backup file name, so renaming a device
    /// moves its history path.
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: DeviceAuth,
    /// Tenant slug. Groups devices in the repository layout and scopes
    /// row-level security in PostgreSQL.
    pub tenant: String,
    pub tags: Vec<String>,
    /// Skipped unless explicitly named.
    pub enabled: bool,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("tenant", &self.tenant)
            .field("enabled", &self.enabled)
            .field("auth", &self.auth)
            .finish()
    }
}

impl Device {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Path of this device's `.rsc` file, relative to the repository root.
    pub fn backup_path(&self, template: &str) -> PathBuf {
        render_backup_path(template, &self.tenant, &self.name, &self.host)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|own| own.eq_ignore_ascii_case(tag))
    }
}

/// How to authenticate to a device over SSH. Secrets are already decrypted.
#[derive(Clone)]
pub enum DeviceAuth {
    Password(String),
    Key {
        private_key: PathBuf,
        passphrase: Option<String>,
    },
    /// A key held by a running `ssh-agent`.
    Agent,
}

impl DeviceAuth {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            Self::Key { .. } => "key",
            Self::Agent => "agent",
        }
    }

    /// The SSH method name, for error messages.
    pub fn method(&self) -> &'static str {
        match self {
            Self::Password(_) => "password",
            Self::Key { .. } => "publickey",
            Self::Agent => "publickey (agent)",
        }
    }
}

impl std::fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind())
    }
}

// ---------------------------------------------------------------------------
// Validation and selection
// ---------------------------------------------------------------------------

impl Config {
    /// Checks that must hold before a run starts.
    ///
    /// Most of these are also enforced by the database, but the engine is used
    /// from tests and from the CLI, so it re-checks rather than trusting its
    /// caller.
    pub fn validate(&self) -> Result<()> {
        if self.general.concurrency == 0 {
            return Err(Error::config("concurrency must be at least 1"));
        }
        if self.backup.repo_path.as_os_str().is_empty() {
            return Err(Error::config(
                "the backup repository path must not be empty",
            ));
        }
        if !self.backup.path_template.contains("{device}") {
            return Err(Error::config(
                "the path template must contain `{device}`, otherwise devices overwrite \
                 each other",
            ));
        }
        if self.backup.path_template.starts_with('/') {
            return Err(Error::config(
                "the path template must be relative to the repository root",
            ));
        }

        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for device in &self.devices {
            if device.name.trim().is_empty() {
                return Err(Error::config("every device needs a name"));
            }
            if !names.insert(device.name.to_lowercase()) {
                return Err(Error::config(format!(
                    "duplicate device name `{}`",
                    device.name
                )));
            }
            if device.host.trim().is_empty() {
                return Err(Error::config(format!(
                    "device `{}` has no host",
                    device.name
                )));
            }
            if device.username.trim().is_empty() {
                return Err(Error::config(format!(
                    "device `{}` has no SSH username",
                    device.name
                )));
            }
            // Two devices writing one file would ping-pong the same path on
            // every run; catch it before it reaches Git history.
            let path = device.backup_path(&self.backup.path_template);
            if !paths.insert(path.clone()) {
                return Err(Error::config(format!(
                    "devices collide on backup path {} — make the path template more specific",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Devices matching `filter`, in configuration order.
    pub fn select(&self, filter: &DeviceFilter) -> Result<Vec<&Device>> {
        let selected: Vec<&Device> = self
            .devices
            .iter()
            .filter(|device| filter.matches(device))
            .collect();

        // An explicit name that matches nothing is a mistake, not an empty
        // fleet; fail instead of reporting a clean no-op run.
        for name in &filter.names {
            if !selected
                .iter()
                .any(|device| device.name.eq_ignore_ascii_case(name))
            {
                return Err(Error::config(format!("no device named `{name}`")));
            }
        }
        Ok(selected)
    }

    pub fn find_device(&self, name: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|device| device.name.eq_ignore_ascii_case(name))
    }

    pub fn repo_path(&self) -> PathBuf {
        expand_tilde(&self.backup.repo_path)
    }
}

/// Device selection for a run. Empty vectors mean "no constraint".
#[derive(Debug, Clone, Default)]
pub struct DeviceFilter {
    pub names: Vec<String>,
    pub tags: Vec<String>,
    pub tenants: Vec<String>,
    /// Include devices that are switched off.
    pub include_disabled: bool,
}

impl DeviceFilter {
    /// Select exactly one device by name, even if it is disabled.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            names: vec![name.into()],
            ..Self::default()
        }
    }

    fn matches(&self, device: &Device) -> bool {
        // Naming a device explicitly overrides `enabled = false`: the operator
        // asked for this one.
        let named = self
            .names
            .iter()
            .any(|name| device.name.eq_ignore_ascii_case(name));
        if !self.names.is_empty() && !named {
            return false;
        }
        if !device.enabled && !self.include_disabled && !named {
            return false;
        }
        if !self.tags.is_empty() && !self.tags.iter().any(|tag| device.has_tag(tag)) {
            return false;
        }
        if !self.tenants.is_empty()
            && !self
                .tenants
                .iter()
                .any(|tenant| device.tenant.eq_ignore_ascii_case(tenant))
        {
            return false;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reduce a string to a safe path component: lowercase `[a-z0-9._-]`, never
/// empty, and never `.` or `..`.
///
/// Device and tenant names arrive from a web form, so they are flattened rather
/// than interpreted: `../../etc/passwd` must become a harmless file name, not a
/// traversal. The dots-only check matters — trimming alone can turn `/../` into
/// a literal `..` component.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            'a'..='z' | '0'..='9' | '-' | '_' | '.' => out.push(ch),
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            _ => out.push('-'),
        }
    }
    let trimmed = out.trim_matches(|c| c == '.' || c == '-');
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Render a repository-relative path from the template.
///
/// Shared with the web layer, which renders the same path from a database row
/// without building a whole [`Device`]. One implementation, so the UI can never
/// disagree with the pipeline about where a file lives.
pub fn render_backup_path(template: &str, tenant: &str, device: &str, host: &str) -> PathBuf {
    let rendered = template
        .replace("{tenant}", &slugify(tenant))
        .replace("{device}", &slugify(device))
        .replace("{host}", &slugify(host));
    // Every segment is already slugified, so the only separators left are the
    // ones the template itself asked for.
    rendered
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

/// Expand a leading `~` using `$HOME`.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device with the boring fields filled in.
    pub(crate) fn device(name: &str, tenant: &str) -> Device {
        Device {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            name: name.to_string(),
            host: "10.0.0.1".to_string(),
            port: 22,
            username: "admin".to_string(),
            auth: DeviceAuth::Agent,
            tenant: tenant.to_string(),
            tags: Vec::new(),
            enabled: true,
        }
    }

    fn config(devices: Vec<Device>) -> Config {
        Config {
            general: General::default(),
            backup: Backup {
                repo_path: PathBuf::from("/tmp/backups"),
                path_template: DEFAULT_PATH_TEMPLATE.to_string(),
                committer: Committer::default(),
                remote: None,
            },
            export: Export::default(),
            devices,
        }
    }

    #[test]
    fn export_command_reflects_mode_and_sensitivity() {
        assert_eq!(Export::default().command_line(), "/export terse");
        assert_eq!(
            Export {
                mode: ExportMode::Compact,
                ..Export::default()
            }
            .command_line(),
            "/export"
        );
        assert_eq!(
            Export {
                mode: ExportMode::Verbose,
                show_sensitive: true,
                ..Export::default()
            }
            .command_line(),
            "/export verbose show-sensitive"
        );
    }

    #[test]
    fn stored_enum_values_round_trip() {
        for mode in [ExportMode::Compact, ExportMode::Terse, ExportMode::Verbose] {
            assert_eq!(ExportMode::parse(mode.as_str()), mode);
        }
        for policy in [
            HostKeyPolicy::Strict,
            HostKeyPolicy::AcceptNew,
            HostKeyPolicy::Off,
        ] {
            assert_eq!(HostKeyPolicy::parse(policy.as_str()), policy);
        }
        // Unrecognised text must not silently disable verification.
        assert_eq!(HostKeyPolicy::parse("nonsense"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn backup_paths_are_slugified_and_cannot_escape_the_repo() {
        let mut device = device("../../etc/passwd", "Acme Corp/../..");
        device.host = "10.0.0.1".into();
        let path = device.backup_path(DEFAULT_PATH_TEMPLATE);
        assert_eq!(
            path,
            PathBuf::from("acme-corp/etc-passwd.rsc"),
            "path components must be flattened, not interpreted"
        );
        assert!(!path.components().any(|part| part.as_os_str() == ".."));

        // Trimming alone would leave a literal ".." component here.
        assert_eq!(slugify("/../"), "unnamed");
        assert_eq!(slugify(".."), "unnamed");
        assert_eq!(slugify("..."), "unnamed");
        assert_eq!(slugify(""), "unnamed");
        assert_eq!(slugify("Core-RTR 01"), "core-rtr-01");
    }

    #[test]
    fn duplicate_names_and_colliding_paths_are_rejected() {
        let mut config = config(vec![device("rtr1", "acme"), device("RTR1", "acme")]);
        assert!(config.validate().is_err(), "duplicate name accepted");

        // Different names that slugify to the same file.
        config.devices = vec![device("rtr-1", "acme"), device("rtr/1", "acme")];
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("collide"), "unexpected error: {error}");
    }

    #[test]
    fn a_template_without_the_device_placeholder_is_rejected() {
        let mut config = config(vec![device("rtr1", "acme")]);
        config.backup.path_template = "{tenant}/config.rsc".into();
        assert!(config.validate().is_err());
        config.backup.path_template = "/absolute/{device}.rsc".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_valid_fleet_passes() {
        let config = config(vec![device("rtr1", "acme"), device("rtr2", "lab")]);
        config.validate().unwrap();
    }

    fn fleet() -> Config {
        let mut core = device("core", "acme");
        core.tags = vec!["core".into(), "milan".into()];
        let mut edge = device("edge", "acme");
        edge.tags = vec!["edge".into()];
        let mut lab = device("lab", "lab");
        lab.enabled = false;
        config(vec![core, edge, lab])
    }

    #[test]
    fn selection_skips_disabled_devices_by_default() {
        let names: Vec<_> = fleet()
            .select(&DeviceFilter::default())
            .unwrap()
            .iter()
            .map(|device| device.name.clone())
            .collect();
        assert_eq!(names, ["core", "edge"]);
    }

    #[test]
    fn naming_a_disabled_device_selects_it_anyway() {
        let config = fleet();
        let selected = config.select(&DeviceFilter::named("lab")).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "lab");
    }

    #[test]
    fn tag_and_tenant_filters_narrow_the_fleet() {
        let config = fleet();
        let by_tag = config
            .select(&DeviceFilter {
                tags: vec!["MILAN".into()],
                ..DeviceFilter::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 1, "tag matching is case-insensitive");

        let by_tenant = config
            .select(&DeviceFilter {
                tenants: vec!["acme".into()],
                ..DeviceFilter::default()
            })
            .unwrap();
        assert_eq!(by_tenant.len(), 2);
    }

    #[test]
    fn a_misspelled_device_name_is_an_error_not_an_empty_run() {
        assert!(fleet().select(&DeviceFilter::named("cor")).is_err());
    }

    #[test]
    fn debug_output_never_contains_a_secret() {
        let mut device = device("rtr1", "acme");
        device.auth = DeviceAuth::Password("hunter2".into());
        let rendered = format!("{device:?}");
        assert!(!rendered.contains("hunter2"), "secret leaked: {rendered}");

        let remote = Remote {
            name: "origin".into(),
            url: "https://github.com/x/y.git".into(),
            branch: "main".into(),
            push: true,
            auth: GitAuth::Token {
                username: "x-access-token".into(),
                token: "github_pat_SECRET".into(),
            },
            allow_invalid_certs: false,
        };
        let rendered = format!("{remote:?}");
        assert!(!rendered.contains("SECRET"), "token leaked: {rendered}");
    }
}
