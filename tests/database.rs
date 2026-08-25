//! Integration test against a real PostgreSQL.
//!
//! Skipped unless `TEST_DATABASE_URL` is set, so `cargo test` still passes on a
//! machine with no database. To run it:
//!
//! ```sh
//! docker run -d --name dondude-pg -e POSTGRES_PASSWORD=dondude \
//!     -e POSTGRES_USER=dondude -e POSTGRES_DB=dondude -p 55432:5432 postgres:17-alpine
//! TEST_DATABASE_URL=postgres://dondude:dondude@127.0.0.1:55432/dondude_test cargo test --test database
//! ```
//!
//! It runs as one test rather than several so the tables can be truncated once
//! and the steps cannot interleave.
//!
//! **This test truncates every table.** Point it at a throwaway database, never
//! at one you are also using by hand. It clears its own ciphertext on the way
//! out too: secrets sealed with the test's disposable key would otherwise sit in
//! `settings` and make a real deployment fail to decrypt, with an error that
//! looks like a lost master key.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use mikrotik_dondude::backup::{DeviceReport, Outcome, PushReport, RunReport};
use mikrotik_dondude::crypto::MasterKey;
use mikrotik_dondude::db::{Db, DeviceInput, SettingsInput};
use sqlx::Row;
use uuid::Uuid;

const ROUTER_PASSWORD: &str = "R0uter-Pa55word!";
const GITHUB_TOKEN: &str = "github_pat_ThisIsNotARealToken";

fn device_input(name: &str, secret: Option<&str>) -> DeviceInput {
    DeviceInput {
        name: name.to_string(),
        host: "10.0.0.1".to_string(),
        port: 22,
        username: "dondude-backup".to_string(),
        tenant: "acme".to_string(),
        tags: vec!["core".to_string()],
        enabled: true,
        auth_kind: "password".to_string(),
        secret: secret.map(str::to_string),
        private_key_path: None,
    }
}

fn settings_input(token: Option<&str>) -> SettingsInput {
    SettingsInput {
        path_template: "{tenant}/{device}.rsc".to_string(),
        committer_name: "DonDude".to_string(),
        committer_email: "dondude@example.org".to_string(),
        remote_url: Some("https://github.com/example/backups.git".to_string()),
        remote_branch: "main".to_string(),
        remote_push: true,
        git_username: "x-access-token".to_string(),
        git_token: token.map(str::to_string),
        export_mode: "terse".to_string(),
        show_sensitive: false,
        concurrency: 4,
        connect_timeout_secs: 5,
        command_timeout_secs: 30,
        host_key_policy: "accept-new".to_string(),
        schedule_enabled: true,
        schedule_hour: 3,
        schedule_minute: 15,
    }
}

#[tokio::test]
async fn the_database_layer_round_trips_a_whole_deployment() {
    let Some(dsn) = common::test_dsn() else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };

    let key_material = MasterKey::generate().unwrap();
    let db = Db::connect(&dsn, 4, MasterKey::from_base64(&key_material).unwrap())
        .await
        .expect("connect");
    db.migrate().await.expect("migrate");

    // Start from a known state; the settings row is recreated by hand because
    // the migration seeds it only once.
    sqlx::query(
        "TRUNCATE users, sessions, tenants, devices, backup_runs, backup_events,
             login_attempts CASCADE",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE settings SET remote_url = NULL, git_token_sealed = NULL,
             schedule_enabled = false WHERE id",
    )
    .execute(db.pool())
    .await
    .unwrap();

    // --- operators and sessions -------------------------------------------
    assert_eq!(db.user_count().await.unwrap(), 0);
    let user_id = db
        .create_user("admin", "correct horse battery")
        .await
        .unwrap();
    assert_eq!(db.user_count().await.unwrap(), 1);

    assert!(
        db.create_user("admin", "another password").await.is_err(),
        "duplicate usernames must be refused"
    );
    assert!(
        db.create_user("shorty", "1234567").await.is_err(),
        "short passwords must be refused"
    );

    assert!(
        db.authenticate("admin", "wrong").await.unwrap().is_none(),
        "a wrong password must not authenticate"
    );
    assert!(
        db.authenticate("ghost", "correct horse battery")
            .await
            .unwrap()
            .is_none(),
        "an unknown user must not authenticate"
    );
    let user = db
        .authenticate("admin", "correct horse battery")
        .await
        .unwrap()
        .expect("correct credentials must authenticate");
    assert_eq!(user.id, user_id);
    assert!(user.last_login_at.is_some(), "login time must be recorded");

    // The password must not be recoverable from the row.
    let hash: String = sqlx::query("SELECT password_hash FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("password_hash");
    assert!(hash.starts_with("$argon2"), "expected an Argon2 PHC string");
    assert!(!hash.contains("correct horse"));

    let token = db
        .create_session(user_id, Some("test-agent"))
        .await
        .unwrap();
    assert_eq!(
        db.session_user(&token).await.unwrap().map(|u| u.id),
        Some(user_id)
    );
    // Only the digest is stored, so the cookie value must not appear in the table.
    let stored: String = sqlx::query("SELECT token_hash FROM sessions")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("token_hash");
    assert_ne!(stored, token, "the raw session token must not be stored");
    assert!(db.session_user("not-a-token").await.unwrap().is_none());
    db.delete_session(&token).await.unwrap();
    assert!(
        db.session_user(&token).await.unwrap().is_none(),
        "a deleted session must stop working"
    );

    // --- login throttling --------------------------------------------------
    // A fresh username is not throttled.
    assert!(
        db.login_lockout("admin", Some("10.1.1.1"))
            .await
            .unwrap()
            .is_none()
    );

    // Nine failures stay under the per-username limit of ten.
    for _ in 0..9 {
        db.record_login_attempt("admin", Some("10.1.1.1"), false)
            .await
            .unwrap();
    }
    assert!(
        db.login_lockout("admin", Some("10.1.1.1"))
            .await
            .unwrap()
            .is_none(),
        "nine failures must not lock the account"
    );

    db.record_login_attempt("admin", Some("10.1.1.1"), false)
        .await
        .unwrap();
    let wait = db
        .login_lockout("admin", Some("10.1.1.1"))
        .await
        .unwrap()
        .expect("ten failures must lock the account");
    assert!(wait > 0 && wait <= 15 * 60, "implausible wait: {wait}s");

    // The lock is per username: another account from the same address is fine
    // until the (higher) per-address limit is reached.
    assert!(
        db.login_lockout("someone-else", Some("10.1.1.1"))
            .await
            .unwrap()
            .is_none(),
        "one locked username must not lock every account"
    );
    // ...and a different address is unaffected.
    assert!(
        db.login_lockout("other", Some("10.9.9.9"))
            .await
            .unwrap()
            .is_none()
    );

    // A success clears that username's failures, so an operator who mistypes a
    // few times is not left locked out afterwards.
    db.record_login_attempt("admin", Some("10.1.1.1"), true)
        .await
        .unwrap();
    assert!(
        db.login_lockout("admin", Some("10.1.1.1"))
            .await
            .unwrap()
            .is_none(),
        "a successful sign-in must clear the throttle"
    );

    // The per-address limit bites across many usernames (a spray).
    for i in 0..30 {
        db.record_login_attempt(&format!("victim-{i}"), Some("10.2.2.2"), false)
            .await
            .unwrap();
    }
    assert!(
        db.login_lockout("victim-0", Some("10.2.2.2"))
            .await
            .unwrap()
            .is_some(),
        "thirty failures from one address must be throttled"
    );
    assert!(
        db.login_lockout("victim-0", Some("10.3.3.3"))
            .await
            .unwrap()
            .is_none(),
        "the address limit must not leak to other addresses"
    );

    // --- the run lock ------------------------------------------------------
    let lock = db
        .try_lock_run()
        .await
        .unwrap()
        .expect("the lock must be free at first");
    assert!(
        db.try_lock_run().await.unwrap().is_none(),
        "a second run must not be able to take the lock"
    );
    // Another connection, standing in for a `dondude backup run` from cron.
    let cli = Db::connect(&dsn, 2, MasterKey::from_base64(&key_material).unwrap())
        .await
        .unwrap();
    assert!(
        cli.try_lock_run().await.unwrap().is_none(),
        "the lock must hold across processes, not just within one"
    );

    drop(lock);
    assert!(
        cli.try_lock_run().await.unwrap().is_some(),
        "dropping the guard must release the lock"
    );

    // --- devices -----------------------------------------------------------
    let device_id = db
        .create_device(&device_input("core-rtr-01", Some(ROUTER_PASSWORD)))
        .await
        .unwrap();

    assert!(
        db.create_device(&device_input("core-rtr-01", Some("x")))
            .await
            .is_err(),
        "duplicate device names in one tenant must be refused"
    );
    assert!(
        db.create_device(&device_input("no-password", None))
            .await
            .is_err(),
        "password auth without a password must be refused"
    );

    let row = db.device(device_id).await.unwrap();
    assert_eq!(row.name, "core-rtr-01");
    assert_eq!(row.tenant, "acme");
    assert_eq!(row.tags, vec!["core".to_string()]);
    assert!(row.has_secret);

    // The password must be unreadable in the database.
    let sealed: String = sqlx::query("SELECT secret_sealed FROM devices WHERE id = $1")
        .bind(device_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("secret_sealed");
    assert!(
        !sealed.contains(ROUTER_PASSWORD),
        "the router password is stored in the clear"
    );

    // --- settings ----------------------------------------------------------
    db.update_settings(&settings_input(Some(GITHUB_TOKEN)))
        .await
        .unwrap();
    let settings = db.settings().await.unwrap();
    assert!(settings.has_git_token);
    assert_eq!(settings.concurrency, 4);
    assert_eq!(settings.schedule_hour, 3);

    let sealed_token: String = sqlx::query("SELECT git_token_sealed FROM settings WHERE id")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("git_token_sealed");
    assert!(
        !sealed_token.contains("github_pat"),
        "the GitHub token is stored in the clear"
    );
    assert_eq!(db.git_token().await.unwrap().as_deref(), Some(GITHUB_TOKEN));

    // Saving again with an empty token keeps the stored one; "" clears it.
    db.update_settings(&settings_input(None)).await.unwrap();
    assert_eq!(db.git_token().await.unwrap().as_deref(), Some(GITHUB_TOKEN));
    db.update_settings(&settings_input(Some(""))).await.unwrap();
    assert_eq!(db.git_token().await.unwrap(), None);
    db.update_settings(&settings_input(Some(GITHUB_TOKEN)))
        .await
        .unwrap();

    assert!(
        db.update_settings(&SettingsInput {
            path_template: "{tenant}/config.rsc".to_string(),
            ..settings_input(None)
        })
        .await
        .is_err(),
        "a template that cannot distinguish devices must be refused"
    );

    // --- runtime config ----------------------------------------------------
    let config = db
        .runtime_config(PathBuf::from("/tmp/dondude-test"))
        .await
        .unwrap();
    assert_eq!(config.devices.len(), 1);
    assert_eq!(config.general.concurrency, 4);
    assert_eq!(config.export.command_line(), "/export terse");

    let device = &config.devices[0];
    match &device.auth {
        mikrotik_dondude::config::DeviceAuth::Password(password) => {
            assert_eq!(password, ROUTER_PASSWORD, "the password must decrypt");
        }
        other => panic!("expected password auth, got {other:?}"),
    }
    let remote = config
        .backup
        .remote
        .as_ref()
        .expect("a remote was configured");
    match &remote.auth {
        mikrotik_dondude::config::GitAuth::Token { token, .. } => {
            assert_eq!(token, GITHUB_TOKEN, "the token must decrypt")
        }
        other => panic!("expected token auth, got {other:?}"),
    }
    // Secrets must not leak through Debug, which is what ends up in logs.
    assert!(!format!("{device:?}").contains(ROUTER_PASSWORD));
    assert!(!format!("{remote:?}").contains(GITHUB_TOKEN));

    // --- editing without retyping the password -----------------------------
    let mut edit = device_input("core-rtr-01", None);
    edit.host = "10.0.0.9".to_string();
    edit.enabled = false;
    db.update_device(device_id, &edit).await.unwrap();

    let row = db.device(device_id).await.unwrap();
    assert_eq!(row.host, "10.0.0.9");
    assert!(!row.enabled);
    let config = db
        .runtime_config(PathBuf::from("/tmp/dondude-test"))
        .await
        .unwrap();
    match &config.devices[0].auth {
        mikrotik_dondude::config::DeviceAuth::Password(password) => assert_eq!(
            password, ROUTER_PASSWORD,
            "an empty password field must keep the stored one"
        ),
        other => panic!("expected password auth, got {other:?}"),
    }

    // A wrong master key must fail loudly rather than hand back rubbish.
    let other_key = Db::connect(
        &dsn,
        2,
        MasterKey::from_base64(&MasterKey::generate().unwrap()).unwrap(),
    )
    .await
    .unwrap();
    let error = other_key
        .runtime_config(PathBuf::from("/tmp/dondude-test"))
        .await
        .expect_err("a different master key must not decrypt");
    assert!(
        error.to_string().contains("DONDUDE_MASTER_KEY"),
        "unhelpful error: {error}"
    );

    // --- run history -------------------------------------------------------
    let run_id = db.start_run("cli", false).await.unwrap();
    let report = RunReport {
        started_at: chrono::Utc::now(),
        elapsed: Duration::from_secs(3),
        devices: vec![DeviceReport {
            device: "core-rtr-01".to_string(),
            device_id,
            tenant_id: row.tenant_id,
            host: "10.0.0.9".to_string(),
            tenant: "acme".to_string(),
            path: PathBuf::from("acme/core-rtr-01.rsc"),
            firmware: Some("7.14.3".to_string()),
            model: Some("RB5009UG+S+".to_string()),
            identity: Some("core".to_string()),
            serial: Some("HGT08".to_string()),
            outcome: Outcome::Unchanged,
            elapsed: Duration::from_millis(1200),
        }],
        sync: None,
        push: PushReport::Skipped("no new commits"),
        dry_run: false,
    };
    db.finish_run(run_id, &report, "line one\nline two")
        .await
        .unwrap();

    let stored_run = db.run(run_id).await.unwrap();
    assert_eq!(stored_run.status, "completed");
    assert_eq!(stored_run.unchanged, 1);
    assert_eq!(stored_run.log, "line one\nline two");

    let events = db.run_events(run_id).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, "unchanged");
    assert_eq!(events[0].device_name, "core-rtr-01");

    // The run must have refreshed what we know about the device.
    let row = db.device(device_id).await.unwrap();
    assert_eq!(row.last_outcome.as_deref(), Some("unchanged"));
    assert_eq!(row.firmware.as_deref(), Some("7.14.3"));
    assert!(row.last_seen_at.is_some());

    // A failure must not blank out the last known firmware.
    let run_id = db.start_run("cli", false).await.unwrap();
    let mut failed = report.clone();
    failed.devices[0].outcome = Outcome::Failed("cannot reach 10.0.0.9:22".to_string());
    failed.devices[0].firmware = None;
    db.finish_run(run_id, &failed, "").await.unwrap();

    let row = db.device(device_id).await.unwrap();
    assert_eq!(row.last_outcome.as_deref(), Some("failed"));
    assert_eq!(
        row.firmware.as_deref(),
        Some("7.14.3"),
        "a failed run must not erase known facts"
    );
    assert_eq!(db.run(run_id).await.unwrap().status, "failed");

    // --- scheduler guard ---------------------------------------------------
    let recent = chrono::Utc::now() - chrono::Duration::minutes(5);
    assert!(!db.scheduled_run_since(recent).await.unwrap());
    let scheduled = db.start_run("schedule", false).await.unwrap();
    assert!(
        db.scheduled_run_since(recent).await.unwrap(),
        "the scheduler must see its own run and not fire twice"
    );

    // --- restart recovery --------------------------------------------------
    assert_eq!(db.run(scheduled).await.unwrap().status, "running");
    db.recover_after_restart().await.unwrap();
    assert_eq!(
        db.run(scheduled).await.unwrap().status,
        "failed",
        "a run orphaned by a restart must not stay 'running' forever"
    );

    // --- deletion ----------------------------------------------------------
    assert_eq!(db.delete_device(device_id).await.unwrap(), "core-rtr-01");
    assert!(db.devices().await.unwrap().is_empty());
    assert!(
        db.device(device_id).await.is_err(),
        "a deleted device must not be findable"
    );
    assert!(
        db.run_events(run_id).await.unwrap().is_empty(),
        "events must be cleaned up with the device"
    );
    // Uuid that never existed.
    assert!(db.device(Uuid::new_v4()).await.is_err());

    // Leave nothing sealed with this run's disposable key.
    sqlx::query(
        "TRUNCATE users, sessions, tenants, devices, backup_runs, backup_events,
             login_attempts CASCADE",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE settings SET git_token_sealed = NULL, remote_url = NULL,
             schedule_enabled = false WHERE id",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let settings = db.settings().await.unwrap();
    assert!(
        !settings.has_git_token,
        "the test must not leave a sealed token behind"
    );
    assert!(
        db.runtime_config(PathBuf::from("/tmp/dondude-test"))
            .await
            .is_ok(),
        "the database must be usable by a real deployment afterwards"
    );
}
