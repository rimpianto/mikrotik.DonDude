//! DonDude — multi-tenant MikroTik RouterOS fleet management.
//!
//! # Phase 1: Git-versioned configuration backups
//!
//! ```text
//!   dondude.toml ──▶ config ──▶ backup (orchestrator)
//!                                 │
//!                                 ├─▶ routeros  SSH + /export, normalized
//!                                 │
//!                                 └─▶ git       diff, commit, push
//! ```
//!
//! The dependency arrows only point one way. [`routeros`] knows nothing about
//! Git; [`git`] knows nothing about RouterOS; [`backup`] is the only module that
//! knows both. Adding a second transport (the RouterOS binary API) or a second
//! sink therefore touches one module, not three.
//!
//! # Design commitments
//!
//! * **Diff stability over fidelity.** A raw `/export` carries a timestamp, so
//!   committing it verbatim would produce a diff per device per run. The banner
//!   is rewritten without the clock — see [`routeros::export`].
//! * **One dead device does not fail the fleet.** Per-device errors are
//!   collected into a report; only bad config or an unusable repository aborts.
//! * **Secrets come from the environment.** Config files name environment
//!   variables; the values are resolved at the start of a run so a missing token
//!   fails before any work is done.
//! * **The backup repository is data, never source.** It lives at its own path
//!   with its own remote, and DonDude refuses to use a Rust source tree for it.

pub mod backup;
pub mod backup_archive;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod git;
pub mod monitor;
pub mod notify;
pub mod routeros;
pub mod web;

pub use config::Config;
pub use error::{Error, Result};

/// Crate version, surfaced by `dondude --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Install the tracing subscriber.
///
/// `RUST_LOG` wins when set; otherwise verbosity comes from the count of `-v`
/// flags, so an operator can raise the level without knowing about env filters.
pub fn init_tracing(verbosity: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;

    let default = if quiet {
        "warn"
    } else {
        match verbosity {
            0 => "mikrotik_dondude=info,warn",
            1 => "mikrotik_dondude=debug,info",
            _ => "mikrotik_dondude=trace,debug",
        }
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(verbosity > 0)
        .with_writer(std::io::stderr)
        .init();
}
