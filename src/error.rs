//! Error types shared across DonDude.
//!
//! The split is deliberate: [`DeviceError`] is *per-device* and therefore
//! recoverable — the backup pipeline records it against a single device and
//! keeps going through the rest of the fleet. Everything in [`Error`] aborts
//! the run because it means the configuration, the backup repository or the
//! process environment is unusable.
//!
//! Variants carrying a `#[source]` deliberately leave the cause out of their own
//! `Display` text. `anyhow`'s `{:#}` and [`chain`] both walk the chain and join
//! it with `": "`, so embedding it as well would print every cause twice.

use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to read config file {path}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file {path}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("environment variable `{var}` is not set (it holds a required secret)")]
    MissingSecret { var: String },

    #[error("device `{name}`")]
    Device {
        name: String,
        #[source]
        source: DeviceError,
    },

    #[error("backup repository {path}")]
    Repo {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },

    #[error(transparent)]
    Git(#[from] git2::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("database migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("{0}")]
    Crypto(String),

    #[error("{0} not found")]
    NotFound(&'static str),
}

impl Error {
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }
}

/// A failure isolated to one device: logged, reported, and skipped.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("cannot reach {addr}")]
    Connect {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("SSH handshake failed")]
    Handshake(#[source] ssh2::Error),

    #[error("authentication failed for user `{user}` (tried: {method})")]
    Auth { user: String, method: &'static str },

    #[error("host key rejected: {0}")]
    HostKey(String),

    #[error("command `{command}` exited with status {status}{}", detail(stderr))]
    Command {
        command: String,
        status: i32,
        stderr: String,
    },

    #[error("`{command}` produced no output; is this a RouterOS device?")]
    EmptyOutput { command: String },

    #[error("timed out after {0:?}")]
    Timeout(Duration),

    #[error("private key {path} is unreadable")]
    KeyFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("environment variable `{var}` is not set (it holds a required secret)")]
    MissingSecret { var: String },

    #[error(transparent)]
    Ssh(#[from] ssh2::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("worker thread panicked")]
    WorkerPanic,
}

fn detail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

impl DeviceError {
    /// Attach a device name so the failure can be reported in a fleet summary.
    pub fn named(self, name: impl Into<String>) -> Error {
        Error::Device {
            name: name.into(),
            source: self,
        }
    }
}

/// Render an error and its whole cause chain on one line.
///
/// Used where an error must be flattened into a string that is no longer an
/// `Error` — a report field, a database column — and would otherwise lose
/// everything below the top frame.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_flattens_causes_exactly_once() {
        let error = DeviceError::Connect {
            addr: "10.0.0.1:22".into(),
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        }
        .named("rtr1");

        // Display alone stops at the top frame...
        assert_eq!(error.to_string(), "device `rtr1`");
        // ...and the chain adds each cause once.
        assert_eq!(
            chain(&error),
            "device `rtr1`: cannot reach 10.0.0.1:22: refused"
        );
    }
}
