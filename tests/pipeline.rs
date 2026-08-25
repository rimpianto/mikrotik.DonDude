//! Seam test: RouterOS export normalization feeding the Git worker.
//!
//! This is the pair of modules the backup pipeline joins, and the property that
//! matters is emergent rather than local — a re-run of an unchanged device must
//! produce no commit, even though the device's raw output differs every time.
//! Covering it without a live router means driving the two modules directly with
//! canned `/export` text.

use std::path::Path;

use chrono::Utc;
use mikrotik_dondude::config::{Backup, Committer, Export};
use mikrotik_dondude::git::{BackupRepo, CommitMeta, Stored};
use mikrotik_dondude::routeros::export;

/// Raw `/export` output, with the clock as a parameter.
fn raw_export(timestamp: &str, version: &str, address: &str) -> String {
    format!(
        "# {timestamp} by RouterOS {version}\n\
         # software id = ABCD-EFGH\n\
         #\n\
         # model = RB5009UG+S+\n\
         # serial number = HGT08XXXXX\n\
         \n\
         /interface bridge\n\
         add name=bridge\n\
         /ip address\n\
         add address={address} interface=bridge\n"
    )
}

fn meta(captured: chrono::DateTime<Utc>) -> CommitMeta {
    CommitMeta {
        device: "core-rtr-01".into(),
        host: "10.0.0.1".into(),
        tenant: "acme".into(),
        firmware: Some("7.13.2".into()),
        model: Some("RB5009UG+S+".into()),
        serial: Some("HGT08XXXXX".into()),
        software_id: Some("ABCD-EFGH".into()),
        identity: Some("core".into()),
        command: "/export terse".into(),
        captured_at: captured,
    }
}

fn repo(path: &Path) -> BackupRepo {
    BackupRepo::open_or_init(&Backup {
        repo_path: path.to_path_buf(),
        path_template: "{tenant}/{device}.rsc".into(),
        committer: Committer::default(),
        remote: None,
    })
    .unwrap()
}

fn capture(raw: &str) -> String {
    export::normalize(raw, "/export terse", "core-rtr-01", &Export::default()).contents
}

#[test]
fn re_running_an_unchanged_device_produces_exactly_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let path = Path::new("acme/core-rtr-01.rsc");

    let first = capture(&raw_export("2024-01-15 10:22:31", "7.13.2", "10.0.0.1/24"));
    let stored = repo.store(path, &first, &meta(Utc::now())).unwrap();
    assert!(stored.commit().is_some(), "the first capture must commit");

    // Ten more runs, each with a different clock. None may commit.
    for hour in 0..10 {
        let raw = raw_export(
            &format!("2024-02-0{hour} 03:14:15"),
            "7.13.2",
            "10.0.0.1/24",
        );
        let outcome = repo.store(path, &capture(&raw), &meta(Utc::now())).unwrap();
        assert!(
            matches!(outcome, Stored::Unchanged),
            "run {hour} committed a clock change"
        );
    }

    assert_eq!(commit_count(dir.path()), 1);
}

#[test]
fn a_real_configuration_change_commits_with_accurate_stats() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let path = Path::new("acme/core-rtr-01.rsc");

    repo.store(
        path,
        &capture(&raw_export("2024-01-15 10:22:31", "7.13.2", "10.0.0.1/24")),
        &meta(Utc::now()),
    )
    .unwrap();

    let changed = capture(&raw_export("2024-01-16 10:22:31", "7.13.2", "10.0.0.9/24"));
    let commit = repo
        .store(path, &changed, &meta(Utc::now()))
        .unwrap()
        .commit()
        .cloned()
        .expect("an address change must commit");
    assert_eq!((commit.insertions, commit.deletions), (1, 1));
    assert!(!commit.initial);
    assert_eq!(commit_count(dir.path()), 2);
}

#[test]
fn a_firmware_upgrade_is_visible_in_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let path = Path::new("acme/core-rtr-01.rsc");

    repo.store(
        path,
        &capture(&raw_export("2024-01-15 10:22:31", "7.13.2", "10.0.0.1/24")),
        &meta(Utc::now()),
    )
    .unwrap();

    let upgraded = capture(&raw_export("2024-03-01 08:00:00", "7.14.3", "10.0.0.1/24"));
    assert!(
        repo.store(path, &upgraded, &meta(Utc::now()))
            .unwrap()
            .commit()
            .is_some(),
        "a firmware change must be recorded"
    );
    let stored = std::fs::read_to_string(dir.path().join(path)).unwrap();
    assert!(stored.contains("# routeros = 7.14.3"));
    // The device's own clock must never reach the file.
    assert!(!stored.contains("08:00:00"));
}

/// Count commits on HEAD without shelling out to `git`.
fn commit_count(path: &Path) -> usize {
    let repo = git2::Repository::open(path).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_head().unwrap();
    walk.count()
}
