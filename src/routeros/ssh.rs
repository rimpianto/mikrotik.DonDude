//! Blocking SSH transport for RouterOS devices.
//!
//! `ssh2` (libssh2) is synchronous, so everything here blocks. Callers must not
//! invoke it from an async context directly — [`super::fetch`] wraps it in
//! `tokio::task::spawn_blocking`. Keeping the blocking code in its own module
//! makes that boundary explicit and keeps the async side free of `libssh2`
//! lifetimes, which are not `Send`-friendly.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use ssh2::{CheckResult, KnownHostFileKind, Session};
use tracing::{debug, warn};

use crate::config::{Device, DeviceAuth, General, HostKeyPolicy};
use crate::error::DeviceError;

/// Everything needed to open one session.
///
/// Credentials arrive already decrypted from the database, so building a target
/// cannot fail.
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: DeviceAuth,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    pub host_key_policy: HostKeyPolicy,
    pub known_hosts: Option<PathBuf>,
}

impl Target {
    /// Build a connectable target from a configured device.
    pub fn from_config(device: &Device, general: &General) -> Self {
        Self {
            host: device.host.clone(),
            port: device.port,
            username: device.username.clone(),
            auth: device.auth.clone(),
            connect_timeout: general.connect_timeout(),
            command_timeout: general.command_timeout(),
            host_key_policy: general.host_key_policy,
            known_hosts: general.known_hosts_path(),
        }
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// The captured result of one remote command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

/// An authenticated SSH session. Blocking; one per device per run.
pub struct SshSession {
    session: Session,
    target: Target,
}

impl SshSession {
    /// Connect, verify the host key, and authenticate.
    pub fn connect(target: Target) -> Result<Self, DeviceError> {
        let addr = target.addr();

        // Resolve explicitly so a DNS failure is reported as such, and so the
        // connect timeout applies per address rather than to the whole set.
        let mut resolved = addr
            .to_socket_addrs()
            .map_err(|source| DeviceError::Connect {
                addr: addr.clone(),
                source,
            })?;
        let socket_addr = resolved.next().ok_or_else(|| DeviceError::Connect {
            addr: addr.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "hostname resolved to no addresses",
            ),
        })?;

        let stream =
            TcpStream::connect_timeout(&socket_addr, target.connect_timeout).map_err(|source| {
                DeviceError::Connect {
                    addr: addr.clone(),
                    source,
                }
            })?;
        stream.set_nodelay(true).ok();

        let mut session = Session::new()?;
        // libssh2 applies this to every blocking read/write, which is what
        // bounds a wedged `/export` mid-transfer.
        session.set_timeout(timeout_millis(target.command_timeout));
        session.set_tcp_stream(stream);
        session.handshake().map_err(DeviceError::Handshake)?;

        let connection = Self { session, target };
        connection.verify_host_key()?;
        connection.authenticate()?;
        Ok(connection)
    }

    fn verify_host_key(&self) -> Result<(), DeviceError> {
        let policy = self.target.host_key_policy;
        if policy == HostKeyPolicy::Off {
            warn!(host = %self.target.host, "host key verification disabled");
            return Ok(());
        }

        let Some((key, key_type)) = self.session.host_key() else {
            return Err(DeviceError::HostKey(
                "server presented no host key".to_string(),
            ));
        };
        let Some(path) = self.target.known_hosts.clone() else {
            return Err(DeviceError::HostKey(
                "no known_hosts file available; set general.known_hosts or host_key_policy = \"off\""
                    .to_string(),
            ));
        };

        let mut known_hosts = self.session.known_hosts()?;
        if path.exists() {
            known_hosts.read_file(&path, KnownHostFileKind::OpenSSH)?;
        }

        match known_hosts.check_port(&self.target.host, self.target.port, key) {
            CheckResult::Match => Ok(()),
            CheckResult::Mismatch => Err(DeviceError::HostKey(format!(
                "recorded key for {} in {} does not match the key offered by the device; \
                 refusing to continue",
                self.target.host,
                path.display()
            ))),
            CheckResult::NotFound => match policy {
                HostKeyPolicy::Strict => Err(DeviceError::HostKey(format!(
                    "{} is not in {} and host_key_policy is \"strict\"",
                    self.target.host,
                    path.display()
                ))),
                _ => {
                    // accept-new: trust on first use, then pin it.
                    let entry = if self.target.port == 22 {
                        self.target.host.clone()
                    } else {
                        format!("[{}]:{}", self.target.host, self.target.port)
                    };
                    known_hosts.add(&entry, key, "added by dondude", key_type.into())?;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    known_hosts.write_file(&path, KnownHostFileKind::OpenSSH)?;
                    warn!(
                        host = %self.target.host,
                        known_hosts = %path.display(),
                        "recorded new host key on first connection"
                    );
                    Ok(())
                }
            },
            CheckResult::Failure => Err(DeviceError::HostKey(
                "host key check could not be performed".to_string(),
            )),
        }
    }

    fn authenticate(&self) -> Result<(), DeviceError> {
        let user = &self.target.username;
        match &self.target.auth {
            DeviceAuth::Password(password) => {
                self.session.userauth_password(user, password).ok();
            }
            DeviceAuth::Key {
                private_key,
                passphrase,
            } => {
                let private_key = crate::config::expand_tilde(private_key);
                if !private_key.exists() {
                    return Err(DeviceError::KeyFile {
                        path: private_key,
                        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
                    });
                }
                // Older libssh2 builds cannot derive the public key from an
                // OpenSSH-format private key, so hand over the sibling `.pub`
                // when it happens to be there.
                let public_key = private_key.with_extension("pub");
                let public_key = public_key.exists().then_some(public_key);
                self.session
                    .userauth_pubkey_file(
                        user,
                        public_key.as_deref(),
                        &private_key,
                        passphrase.as_deref(),
                    )
                    .ok();
            }
            DeviceAuth::Agent => {
                self.session.userauth_agent(user).ok();
            }
        }

        // libssh2 reports partial-success cases through `authenticated()`, so
        // trust that rather than the per-method return value.
        if self.session.authenticated() {
            Ok(())
        } else {
            Err(DeviceError::Auth {
                user: user.clone(),
                method: self.target.auth.method(),
            })
        }
    }

    /// Run one command and capture stdout, stderr and the exit status.
    ///
    /// RouterOS serves each `exec` on its own channel, so this may be called
    /// repeatedly on the same session.
    pub fn exec(&self, command: &str) -> Result<CommandOutput, DeviceError> {
        debug!(host = %self.target.host, %command, "exec");
        let mut channel = self.session.channel_session()?;
        channel.exec(command)?;

        let mut stdout = Vec::new();
        channel.read_to_end(&mut stdout)?;
        let mut stderr = String::new();
        channel.stderr().read_to_string(&mut stderr).ok();

        channel.send_eof().ok();
        channel.wait_close().ok();
        let status = channel.exit_status().unwrap_or(0);

        Ok(CommandOutput {
            // RouterOS emits its own banner text that is not always valid
            // UTF-8 on older releases; lossy keeps the run alive.
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr,
            status,
        })
    }

    /// Download a remote file over the same session, reading it to end.
    ///
    /// RouterOS 7.16+ serves files over SFTP; older releases only accept the
    /// legacy SCP protocol, which needs the `ftp` user policy either way (the
    /// device file system *is* the FTP service). Try SFTP first, then SCP.
    ///
    /// Missing files surface as an `ssh2` error from whichever transport was
    /// reached; callers that treat "no such file" as an expected case match on
    /// the error there.
    pub fn download_file(&self, remote_path: &str) -> Result<Vec<u8>, DeviceError> {
        debug!(host = %self.target.host, remote_path, "file download");
        let sftp = self.session.sftp();
        if let Ok(sftp) = &sftp
            && let Ok(mut file) = sftp.open(std::path::Path::new(remote_path))
        {
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                return Ok(bytes);
            }
        }
        // Fallback: legacy SCP protocol.
        let (mut channel, _stats) = self.session.scp_recv(std::path::Path::new(remote_path))?;
        let mut bytes = Vec::new();
        channel.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Run a command, failing if it exits non-zero or produces nothing.
    pub fn exec_checked(&self, command: &str) -> Result<String, DeviceError> {
        let output = self.exec(command)?;
        if output.status != 0 {
            return Err(DeviceError::Command {
                command: command.to_string(),
                status: output.status,
                stderr: output.stderr,
            });
        }
        if output.stdout.trim().is_empty() {
            return Err(DeviceError::EmptyOutput {
                command: command.to_string(),
            });
        }
        Ok(output.stdout)
    }
}

/// libssh2 takes milliseconds as `u32`; saturate rather than wrap to 0, since a
/// timeout of 0 means "block forever".
fn timeout_millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}
