//! Git versioning worker for the backup repository.
//!
//! This module owns the *data* repository — the working tree full of `.rsc`
//! files — and nothing else. It is deliberately ignorant of RouterOS: it is
//! handed a relative path, some bytes, and metadata to record.
//!
//! ## Change detection
//!
//! A capture is compared against the file already in the working tree before
//! anything is written. Unchanged devices produce no write, no commit, and no
//! mtime churn, which is what keeps a nightly run over a large fleet quiet
//! enough that a commit actually means something.
//!
//! The working tree alone is not quite enough, though: a run interrupted
//! between write and commit leaves the tree ahead of `HEAD`. So the Git status
//! of the path is consulted too, and a dirty path is committed even when the
//! bytes match.
//!
//! ## One commit per device
//!
//! Each device commits separately, so `git log -- <tenant>/<device>.rsc` is a
//! device's real change history and a commit message can describe exactly one
//! router. The push happens once, after the fleet has been walked.

pub mod auth;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use git2::build::CheckoutBuilder;
use git2::{
    BranchType, Diff, DiffOptions, ErrorCode, Oid, PushOptions, Repository, Signature, Time,
};
use tracing::{debug, info, warn};

use crate::config::{Backup, Committer, Remote};
use crate::error::{Error, Result};

/// What happened to one device's file.
#[derive(Debug, Clone)]
pub enum Stored {
    /// Byte-identical to what is already committed.
    Unchanged,
    /// Written and committed.
    Committed(Commit),
}

impl Stored {
    pub fn commit(&self) -> Option<&Commit> {
        match self {
            Self::Committed(commit) => Some(commit),
            Self::Unchanged => None,
        }
    }
}

/// A commit this run created.
#[derive(Debug, Clone)]
pub struct Commit {
    pub id: Oid,
    pub path: PathBuf,
    pub insertions: usize,
    pub deletions: usize,
    /// First time this device has been backed up.
    pub initial: bool,
}

impl Commit {
    /// Short form for CLI summaries, e.g. `+12 -3`.
    pub fn stats(&self) -> String {
        if self.initial {
            format!("initial, {} lines", self.insertions)
        } else {
            format!("+{} -{}", self.insertions, self.deletions)
        }
    }
}

/// One commit that touched a device's configuration file.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub id: String,
    pub summary: String,
    pub body: String,
    pub author: String,
    pub when: DateTime<Utc>,
    pub insertions: usize,
    pub deletions: usize,
}

impl HistoryEntry {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

/// A line of a unified diff, classified for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Context,
    Hunk,
    Header,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Metadata recorded in the commit for one capture.
#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub device: String,
    pub host: String,
    pub tenant: String,
    pub firmware: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub software_id: Option<String>,
    pub identity: Option<String>,
    pub command: String,
    pub captured_at: DateTime<Utc>,
}

impl CommitMeta {
    /// Render the commit message: a one-line subject plus `key: value`
    /// trailers, so `git log --format` and `git log --grep` can both mine it.
    fn message(&self, summary: &str) -> String {
        let mut message = format!("backup({}): {}\n\n", self.device, summary);
        let mut trailer = |key: &str, value: Option<&str>| {
            if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
                message.push_str(&format!("{key}: {value}\n"));
            }
        };
        trailer("Device", Some(&self.device));
        trailer("Host", Some(&self.host));
        trailer("Tenant", Some(&self.tenant));
        trailer("Identity", self.identity.as_deref());
        trailer("RouterOS", self.firmware.as_deref());
        trailer("Model", self.model.as_deref());
        trailer("Serial", self.serial.as_deref());
        trailer("Software-Id", self.software_id.as_deref());
        trailer("Command", Some(&self.command));
        trailer(
            "Captured",
            Some(
                &self
                    .captured_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        );
        message
    }
}

/// Result of reconciling the local repository with its remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    /// Local branch already matches the remote.
    UpToDate,
    /// Local branch was moved forward to the remote's commit.
    FastForwarded,
    /// Local has commits the remote does not; the push will carry them.
    LocalAhead,
    /// The remote has no such branch yet; the first push creates it.
    RemoteBranchMissing,
    /// Fetch failed. Captures still commit locally; the push will likely fail.
    Unavailable(String),
}

/// The backup repository: a Git working tree of `.rsc` files.
pub struct BackupRepo {
    repo: Repository,
    path: PathBuf,
    branch: String,
    committer: Committer,
}

impl BackupRepo {
    /// Open the backup repository, creating and initializing it if needed.
    pub fn open_or_init(config: &Backup) -> Result<Self> {
        let path = crate::config::expand_tilde(&config.repo_path);

        // The single most damaging misconfiguration would be pointing this at
        // the DonDude checkout, which would commit generated `.rsc` files into
        // the source history. Refuse outright.
        if path.join("Cargo.toml").exists() {
            return Err(Error::config(format!(
                "{} looks like a Rust source tree; backup.repo_path must be a separate \
                 data repository",
                path.display()
            )));
        }

        std::fs::create_dir_all(&path)?;
        let repo = match Repository::open(&path) {
            Ok(repo) => repo,
            Err(error) if error.code() == ErrorCode::NotFound => {
                info!(path = %path.display(), "initializing backup repository");
                Repository::init(&path).map_err(|source| Error::Repo {
                    path: path.clone(),
                    source,
                })?
            }
            Err(source) => {
                return Err(Error::Repo {
                    path: path.clone(),
                    source,
                });
            }
        };

        let backup = Self {
            repo,
            path,
            branch: config.branch().to_string(),
            committer: config.committer.clone(),
        };
        backup.ensure_branch()?;
        Ok(backup)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Point `HEAD` at the configured branch, creating it if necessary.
    fn ensure_branch(&self) -> Result<()> {
        let refname = format!("refs/heads/{}", self.branch);
        match self.repo.head() {
            // Fresh repository: HEAD can simply be aimed at the branch, which
            // the first commit then creates.
            Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
                self.repo.set_head(&refname)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
            Ok(head) => {
                if head.shorthand().ok() == Some(self.branch.as_str()) {
                    return Ok(());
                }
                if self
                    .repo
                    .find_branch(&self.branch, BranchType::Local)
                    .is_err()
                {
                    let commit = head.peel_to_commit()?;
                    self.repo.branch(&self.branch, &commit, false)?;
                }
                self.repo.set_head(&refname)?;
                // Safe checkout: refuse rather than discard anything an
                // operator left in the tree.
                self.repo.checkout_head(None)?;
                Ok(())
            }
        }
    }

    /// True if the working tree or index has changes (ignored files aside).
    pub fn is_dirty(&self) -> Result<bool> {
        let mut options = git2::StatusOptions::new();
        options.include_ignored(false).include_untracked(true);
        Ok(!self.repo.statuses(Some(&mut options))?.is_empty())
    }

    pub fn head_commit(&self) -> Option<Oid> {
        self.repo.head().ok()?.peel_to_commit().ok().map(|c| c.id())
    }

    /// Fetch the remote branch and fast-forward onto it when possible.
    ///
    /// Called before the fleet walk so a second machine, or a fresh clone-less
    /// `repo_path`, builds on the existing history instead of forking it.
    ///
    /// A fetch failure is reported, not raised: captures are worth committing
    /// locally even when the network is down.
    pub fn sync(&self, remote_config: &Remote) -> Result<Synced> {
        self.ensure_remote(remote_config)?;
        let mut remote = self.repo.find_remote(&remote_config.name)?;

        let refspec = format!(
            "+refs/heads/{branch}:refs/remotes/{name}/{branch}",
            branch = self.branch,
            name = remote_config.name
        );
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(auth::callbacks(&remote_config.auth));

        if let Err(error) = remote.fetch(&[refspec.as_str()], Some(&mut options), None) {
            warn!(%error, "could not fetch the backup remote; continuing locally");
            return Ok(Synced::Unavailable(error.message().to_string()));
        }

        let remote_ref = format!("refs/remotes/{}/{}", remote_config.name, self.branch);
        let Ok(reference) = self.repo.find_reference(&remote_ref) else {
            return Ok(Synced::RemoteBranchMissing);
        };
        let remote_oid = reference.peel_to_commit()?.id();

        let Some(local_oid) = self.head_commit() else {
            // Unborn local branch: adopt the remote history wholesale. The
            // working tree is empty, so a forced checkout cannot lose work.
            self.repo.reference(
                &format!("refs/heads/{}", self.branch),
                remote_oid,
                true,
                "dondude: adopt remote branch",
            )?;
            self.repo.set_head(&format!("refs/heads/{}", self.branch))?;
            self.repo
                .checkout_head(Some(CheckoutBuilder::new().force()))?;
            return Ok(Synced::FastForwarded);
        };

        if local_oid == remote_oid {
            return Ok(Synced::UpToDate);
        }
        if self.repo.graph_descendant_of(local_oid, remote_oid)? {
            return Ok(Synced::LocalAhead);
        }
        if !self.repo.graph_descendant_of(remote_oid, local_oid)? {
            // Two shapes of trouble that need different fixes, so they get
            // different messages. Unrelated histories are the common one: the
            // operator ran a backup before configuring the remote, and the
            // remote already had a commit of its own (a README, typically).
            let path = self.path.display();
            let (remote_name, branch) = (&remote_config.name, &self.branch);
            let unrelated = self.repo.merge_base(local_oid, remote_oid).is_err();

            return Err(Error::config(if unrelated {
                format!(
                    "the backup repository {path} and {remote_name}/{branch} have unrelated \
                     histories — neither contains the other's commits. This usually means \
                     backups were committed locally before the remote was configured, and the \
                     remote already had a commit of its own. Either keep the local history \
                     with `git -C {path} rebase --onto {remote_name}/{branch} --root`, or \
                     adopt the remote and let the next run re-capture with `git -C {path} \
                     fetch {remote_name} && git -C {path} reset --hard {remote_name}/{branch}`"
                )
            } else {
                format!(
                    "the backup repository {path} has diverged from {remote_name}/{branch}: \
                     both gained commits since they last agreed. Reconcile them with git — \
                     `git -C {path} pull --rebase {remote_name} {branch}` — then run again"
                )
            }));
        }

        // Fast-forward. Only safe with a clean tree, so check first.
        if self.is_dirty()? {
            return Err(Error::config(format!(
                "backup repository {} has uncommitted changes and is behind {}/{}; \
                 commit or discard them first",
                self.path.display(),
                remote_config.name,
                self.branch
            )));
        }
        self.repo.reference(
            &format!("refs/heads/{}", self.branch),
            remote_oid,
            true,
            "dondude: fast-forward",
        )?;
        self.repo
            .checkout_head(Some(CheckoutBuilder::new().force()))?;
        info!(commit = %remote_oid, "fast-forwarded backup repository");
        Ok(Synced::FastForwarded)
    }

    /// Write a capture and commit it if anything actually changed.
    ///
    /// `relative_path` is interpreted inside the repository; it must not escape
    /// the working tree.
    pub fn store(&self, relative_path: &Path, contents: &str, meta: &CommitMeta) -> Result<Stored> {
        let absolute = self.resolve(relative_path)?;
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let existing = std::fs::read_to_string(&absolute).ok();
        let bytes_changed = existing.as_deref() != Some(contents);
        if bytes_changed {
            std::fs::write(&absolute, contents)?;
        }

        // Even with identical bytes the path can be dirty — an earlier run may
        // have been killed between the write and the commit.
        let status = self
            .repo
            .status_file(relative_path)
            .unwrap_or_else(|_| git2::Status::empty());
        if !bytes_changed && status.is_empty() {
            debug!(path = %relative_path.display(), "unchanged");
            return Ok(Stored::Unchanged);
        }

        let initial = !self.is_tracked(relative_path);
        let commit = self.commit_path(relative_path, meta, initial)?;
        info!(
            device = %meta.device,
            path = %relative_path.display(),
            commit = %commit.id,
            stats = %commit.stats(),
            "committed"
        );
        Ok(Stored::Committed(commit))
    }

    /// Would [`store`](Self::store) change anything? Used by `--dry-run`, which
    /// must not touch the working tree.
    pub fn would_change(&self, relative_path: &Path, contents: &str) -> Result<bool> {
        let absolute = self.resolve(relative_path)?;
        let existing = std::fs::read_to_string(&absolute).ok();
        if existing.as_deref() != Some(contents) {
            return Ok(true);
        }
        Ok(!self
            .repo
            .status_file(relative_path)
            .unwrap_or_else(|_| git2::Status::empty())
            .is_empty())
    }

    fn is_tracked(&self, relative_path: &Path) -> bool {
        self.repo
            .head()
            .and_then(|head| head.peel_to_tree())
            .map(|tree| tree.get_path(relative_path).is_ok())
            .unwrap_or(false)
    }

    fn commit_path(
        &self,
        relative_path: &Path,
        meta: &CommitMeta,
        initial: bool,
    ) -> Result<Commit> {
        let mut index = self.repo.index()?;
        index.add_path(relative_path)?;
        index.write()?;
        let tree = self.repo.find_tree(index.write_tree()?)?;

        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parent_tree = parent.as_ref().and_then(|c| c.tree().ok());

        let diff = self.diff_for(parent_tree.as_ref(), relative_path)?;
        let stats = diff.stats()?;
        let summary = if initial {
            format!("initial capture ({} lines)", stats.insertions())
        } else {
            format!("+{} -{} lines", stats.insertions(), stats.deletions())
        };
        let firmware = meta
            .firmware
            .as_deref()
            .map(|v| format!(" [RouterOS {v}]"))
            .unwrap_or_default();

        // Commit timestamps mirror the capture, so `git log` reads as a
        // timeline of the fleet rather than of the runner's wall clock.
        let when = Time::new(meta.captured_at.timestamp(), 0);
        let signature = Signature::new(&self.committer.name, &self.committer.email, &when)?;

        let id = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            &meta.message(&format!("{summary}{firmware}")),
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )?;

        Ok(Commit {
            id,
            path: relative_path.to_path_buf(),
            insertions: stats.insertions(),
            deletions: stats.deletions(),
            initial,
        })
    }

    fn diff_for(&self, parent_tree: Option<&git2::Tree<'_>>, path: &Path) -> Result<Diff<'_>> {
        let index = self.repo.index()?;
        let mut options = DiffOptions::new();
        options.pathspec(path);
        Ok(self
            .repo
            .diff_tree_to_index(parent_tree, Some(&index), Some(&mut options))?)
    }

    /// Push the configured branch.
    ///
    /// libgit2 reports a *rejected* ref through a callback rather than an error
    /// return, so rejections are collected and turned into a failure here —
    /// otherwise a non-fast-forward push would look like success.
    pub fn push(&self, remote_config: &Remote) -> Result<()> {
        self.ensure_remote(remote_config)?;
        let mut remote = self.repo.find_remote(&remote_config.name)?;

        let rejections = std::cell::RefCell::new(Vec::<String>::new());
        let refspec = format!(
            "refs/heads/{branch}:refs/heads/{branch}",
            branch = self.branch
        );

        // Scoped so the callbacks (which borrow `rejections`) are dropped
        // before the collected rejections are read back out.
        {
            let mut callbacks = auth::callbacks(&remote_config.auth);
            callbacks.push_update_reference(|reference, status| {
                if let Some(reason) = status {
                    rejections
                        .borrow_mut()
                        .push(format!("{reference}: {reason}"));
                }
                Ok(())
            });
            let mut options = PushOptions::new();
            options.remote_callbacks(callbacks);
            remote.push(&[refspec.as_str()], Some(&mut options))?;
        }

        let rejections = rejections.into_inner();
        if !rejections.is_empty() {
            return Err(Error::Config(format!(
                "remote rejected the push ({}); the remote branch has probably moved — \
                 run again to fetch and fast-forward",
                rejections.join("; ")
            )));
        }
        info!(
            remote = %remote_config.name,
            branch = %self.branch,
            "pushed backups"
        );
        Ok(())
    }

    /// Create the remote, or repoint it if the configured URL changed.
    fn ensure_remote(&self, remote_config: &Remote) -> Result<()> {
        match self.repo.find_remote(&remote_config.name) {
            Ok(existing) => {
                if existing.url().ok() != Some(remote_config.url.as_str()) {
                    warn!(
                        remote = %remote_config.name,
                        url = %remote_config.url,
                        "updating remote URL to match the config"
                    );
                    self.repo
                        .remote_set_url(&remote_config.name, &remote_config.url)?;
                }
                Ok(())
            }
            Err(_) => {
                self.repo.remote(&remote_config.name, &remote_config.url)?;
                Ok(())
            }
        }
    }

    /// Commits that touched one device's file, newest first.
    ///
    /// This is what makes the backup repository useful rather than merely
    /// present: `history` plus [`diff`](Self::diff) answer "what changed on this
    /// router, and when".
    pub fn history(&self, relative_path: &Path, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.resolve(relative_path)?;
        if self.head_commit().is_none() {
            return Ok(Vec::new());
        }

        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        walk.set_sorting(git2::Sort::TIME)?;

        let mut entries = Vec::new();
        for oid in walk {
            if entries.len() >= limit {
                break;
            }
            let commit = self.repo.find_commit(oid?)?;
            let new_tree = commit.tree()?;
            let old_tree = match commit.parent(0) {
                Ok(parent) => Some(parent.tree()?),
                Err(_) => None,
            };

            let mut options = DiffOptions::new();
            options.pathspec(relative_path);
            let diff = self.repo.diff_tree_to_tree(
                old_tree.as_ref(),
                Some(&new_tree),
                Some(&mut options),
            )?;
            // Nothing in this commit touched the file: not part of its history.
            if diff.deltas().len() == 0 {
                continue;
            }
            let stats = diff.stats()?;

            entries.push(HistoryEntry {
                id: commit.id().to_string(),
                summary: commit
                    .summary()
                    .ok()
                    .flatten()
                    .unwrap_or("(no summary)")
                    .to_string(),
                body: commit.body().ok().flatten().unwrap_or_default().to_string(),
                author: commit.author().name().unwrap_or("unknown").to_string(),
                when: DateTime::from_timestamp(commit.time().seconds(), 0).unwrap_or_else(Utc::now),
                insertions: stats.insertions(),
                deletions: stats.deletions(),
            });
        }
        Ok(entries)
    }

    /// The contents of one device's file at a given commit.
    pub fn file_at(&self, commit: &str, relative_path: &Path) -> Result<String> {
        self.resolve(relative_path)?;
        let object = self.repo.revparse_single(commit)?;
        let tree = object.peel_to_tree()?;
        let entry = tree
            .get_path(relative_path)
            .map_err(|_| Error::NotFound("file at that revision"))?;
        let blob = self.repo.find_blob(entry.id())?;
        Ok(String::from_utf8_lossy(blob.content()).into_owned())
    }

    /// Unified diff of one device's file between two commits.
    ///
    /// `from` defaults to the first parent of `to`, which is what "show me this
    /// change" means in the UI.
    pub fn diff(
        &self,
        from: Option<&str>,
        to: &str,
        relative_path: &Path,
    ) -> Result<Vec<DiffLine>> {
        self.resolve(relative_path)?;
        let new_commit = self.repo.revparse_single(to)?.peel_to_commit()?;
        let old_commit = match from {
            Some(rev) => Some(self.repo.revparse_single(rev)?.peel_to_commit()?),
            None => new_commit.parent(0).ok(),
        };

        let new_tree = new_commit.tree()?;
        let old_tree = match &old_commit {
            Some(commit) => Some(commit.tree()?),
            None => None,
        };

        let mut options = DiffOptions::new();
        options.pathspec(relative_path).context_lines(3);
        let diff =
            self.repo
                .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut options))?;

        let mut lines = Vec::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let text = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();
            lines.push(DiffLine {
                kind: match line.origin() {
                    '+' => DiffKind::Added,
                    '-' => DiffKind::Removed,
                    'H' | 'F' => DiffKind::Header,
                    '@' => DiffKind::Hunk,
                    _ => DiffKind::Context,
                },
                text,
            });
            true
        })?;
        Ok(lines)
    }

    /// Join a relative path onto the working tree, rejecting anything that
    /// climbs out of it.
    fn resolve(&self, relative_path: &Path) -> Result<PathBuf> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::config(format!(
                "backup path {} must stay inside the repository",
                relative_path.display()
            )));
        }
        Ok(self.path.join(relative_path))
    }
}

/// Check that a remote is reachable with these credentials.
///
/// Connects and lists refs without creating a working tree, which is what makes
/// it usable as a "test connection" button. Read access is proven; write access
/// is not — only a real push can show that.
pub fn probe_remote(url: &str, branch: &str, auth: &crate::config::GitAuth) -> Result<String> {
    let mut remote = git2::Remote::create_detached(url)?;
    let callbacks = auth::callbacks(auth);
    remote.connect_auth(git2::Direction::Fetch, Some(callbacks), None)?;

    let wanted = format!("refs/heads/{branch}");
    let heads = remote.list()?;
    let branches = heads
        .iter()
        .filter(|head| head.name().starts_with("refs/heads/"))
        .count();
    let found = heads.iter().any(|head| head.name() == wanted);
    // `list()` borrows the connection, so finish reading before disconnecting.
    let message = if branches == 0 {
        format!("Connected. The repository is empty; the first push will create `{branch}`.")
    } else if found {
        format!("Connected. Branch `{branch}` exists ({branches} branch(es) total).")
    } else {
        format!(
            "Connected, but `{branch}` does not exist yet ({branches} other branch(es)); \
             the first push will create it."
        )
    };
    remote.disconnect()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitAuth;

    fn meta(device: &str) -> CommitMeta {
        CommitMeta {
            device: device.to_string(),
            host: "10.0.0.1".into(),
            tenant: "acme".into(),
            firmware: Some("7.13.2".into()),
            model: Some("RB5009UG+S+".into()),
            serial: Some("HGT08".into()),
            software_id: Some("ABCD-EFGH".into()),
            identity: Some("core".into()),
            command: "/export terse".into(),
            captured_at: Utc::now(),
        }
    }

    fn backup_config(path: &Path) -> Backup {
        Backup {
            repo_path: path.to_path_buf(),
            path_template: "{tenant}/{device}.rsc".into(),
            committer: Committer::default(),
            remote: None,
        }
    }

    #[test]
    fn first_store_creates_a_commit_and_a_second_identical_store_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let repo = BackupRepo::open_or_init(&backup_config(dir.path())).unwrap();
        let path = Path::new("acme/rtr1.rsc");

        let first = repo.store(path, "/ip address\n", &meta("rtr1")).unwrap();
        let commit = first.commit().expect("initial store must commit");
        assert!(commit.initial);
        assert_eq!(commit.insertions, 1);

        let second = repo.store(path, "/ip address\n", &meta("rtr1")).unwrap();
        assert!(matches!(second, Stored::Unchanged));
        assert!(!repo.is_dirty().unwrap());
    }

    #[test]
    fn changed_config_commits_with_line_stats() {
        let dir = tempfile::tempdir().unwrap();
        let repo = BackupRepo::open_or_init(&backup_config(dir.path())).unwrap();
        let path = Path::new("acme/rtr1.rsc");

        repo.store(path, "a\nb\nc\n", &meta("rtr1")).unwrap();
        let changed = repo.store(path, "a\nB\nc\nd\n", &meta("rtr1")).unwrap();
        let commit = changed.commit().expect("changed config must commit");
        assert!(!commit.initial);
        assert_eq!((commit.insertions, commit.deletions), (2, 1));
    }

    #[test]
    fn a_dirty_worktree_is_committed_even_when_bytes_match() {
        // Simulates a run killed between writing the file and committing it.
        let dir = tempfile::tempdir().unwrap();
        let repo = BackupRepo::open_or_init(&backup_config(dir.path())).unwrap();
        let path = Path::new("acme/rtr1.rsc");
        repo.store(path, "a\n", &meta("rtr1")).unwrap();

        std::fs::write(dir.path().join(path), "a\nb\n").unwrap();
        let outcome = repo.store(path, "a\nb\n", &meta("rtr1")).unwrap();
        assert!(
            outcome.commit().is_some(),
            "expected the stray edit to be committed"
        );
        assert!(!repo.is_dirty().unwrap());
    }

    #[test]
    fn commit_message_carries_device_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let repo = BackupRepo::open_or_init(&backup_config(dir.path())).unwrap();
        repo.store(Path::new("acme/rtr1.rsc"), "x\n", &meta("rtr1"))
            .unwrap();

        let head = repo.repo.head().unwrap().peel_to_commit().unwrap();
        let message = head.message().unwrap();
        assert!(message.starts_with("backup(rtr1): initial capture (1 lines) [RouterOS 7.13.2]"));
        assert!(message.contains("\nDevice: rtr1\n"));
        assert!(message.contains("\nRouterOS: 7.13.2\n"));
        assert!(message.contains("\nSerial: HGT08\n"));
        assert!(message.contains("\nCommand: /export terse\n"));
    }

    #[test]
    fn paths_cannot_escape_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = BackupRepo::open_or_init(&backup_config(dir.path())).unwrap();
        assert!(
            repo.store(Path::new("../escape.rsc"), "x\n", &meta("rtr1"))
                .is_err()
        );
    }

    #[test]
    fn refuses_to_use_a_source_tree_as_the_backup_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert!(BackupRepo::open_or_init(&backup_config(dir.path())).is_err());
    }

    #[test]
    fn sync_then_push_round_trips_through_a_local_remote() {
        let remote_dir = tempfile::tempdir().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut config = backup_config(dir.path());
        let remote = Remote {
            name: "origin".into(),
            url: remote_dir.path().to_string_lossy().into_owned(),
            branch: "main".into(),
            push: true,
            auth: GitAuth::None,
        };
        config.remote = Some(remote.clone());

        let repo = BackupRepo::open_or_init(&config).unwrap();
        // Nothing on the remote yet.
        assert_eq!(repo.sync(&remote).unwrap(), Synced::RemoteBranchMissing);

        repo.store(Path::new("acme/rtr1.rsc"), "a\n", &meta("rtr1"))
            .unwrap();
        repo.push(&remote).unwrap();

        // A second working tree, pointed at the same remote, adopts the history.
        let other_dir = tempfile::tempdir().unwrap();
        let mut other_config = backup_config(other_dir.path());
        other_config.remote = Some(remote.clone());
        let other = BackupRepo::open_or_init(&other_config).unwrap();
        assert_eq!(other.sync(&remote).unwrap(), Synced::FastForwarded);
        assert_eq!(
            std::fs::read_to_string(other_dir.path().join("acme/rtr1.rsc")).unwrap(),
            "a\n"
        );

        // And an unchanged device on the second machine stays quiet.
        assert!(matches!(
            other
                .store(Path::new("acme/rtr1.rsc"), "a\n", &meta("rtr1"))
                .unwrap(),
            Stored::Unchanged
        ));
    }
}
