//! Credentials for the backup remote.
//!
//! libgit2 asks for credentials through a callback and will ask *repeatedly*
//! until one is accepted or the callback errors. Returning the same rejected
//! credential forever turns a bad token into a hang, so attempts are counted.

use std::cell::Cell;

use git2::{CertificateCheckStatus, Cred, RemoteCallbacks};
use tracing::{debug, warn};

use crate::config::GitAuth;

/// Maximum credential offers per connection before we call it a failure.
const MAX_ATTEMPTS: usize = 3;

/// Build callbacks that answer libgit2's credential requests.
///
/// The returned value borrows `auth`, so keep it alive for the whole
/// fetch or push.
pub fn callbacks(auth: &GitAuth, allow_invalid_certs: bool) -> RemoteCallbacks<'_> {
    let attempts = Cell::new(0usize);
    let mut callbacks = RemoteCallbacks::new();

    if allow_invalid_certs {
        // Accepts any certificate, which is the point: a self-hosted instance
        // with a self-signed certificate cannot be verified. Logged on every
        // connection so it never becomes invisible.
        callbacks.certificate_check(|_certificate, host| {
            warn!(
                host,
                "accepting an unverified TLS certificate because the remote is configured \
                 to allow it"
            );
            Ok(CertificateCheckStatus::CertificateOk)
        });
    }

    callbacks.credentials(move |url, username_from_url, allowed| {
        let attempt = attempts.get() + 1;
        attempts.set(attempt);
        if attempt > MAX_ATTEMPTS {
            return Err(git2::Error::from_str(
                "the remote rejected every credential offered — check the repository URL and \
                 the access token in Settings",
            ));
        }
        debug!(attempt, %url, ?allowed, "offering credentials");

        match auth {
            // HTTP basic. GitHub ignores the username when the password is a
            // token; Gitea and friends check it.
            GitAuth::Token { username, token } => Cred::userpass_plaintext(username, token),
            GitAuth::None => {
                let config = git2::Config::open_default()?;
                Cred::credential_helper(&config, url, username_from_url)
            }
        }
    });

    callbacks
}
