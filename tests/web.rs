//! HTTP-level tests for the web interface.
//!
//! Skipped unless `TEST_DATABASE_URL` is set (and it **truncates every table**,
//! so point it at a throwaway database).
//!
//! These exist because two bugs of the same shape got through hand testing: a
//! form that re-rendered from stored state instead of from what was submitted,
//! so the operator's input silently vanished and the next save wrote nothing.
//! Unit tests cannot see that — it only shows up in the request/response round
//! trip.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use mikrotik_dondude::crypto::MasterKey;
use mikrotik_dondude::db::Db;
use mikrotik_dondude::web::{AppState, router};
use tower::ServiceExt;

const REPO_URL: &str = "https://github.com/example/mikrotik-backups.git";

/// Every field the settings form posts, so the extractor cannot fail for a
/// reason unrelated to what a test is checking.
fn settings_body(remote_url: &str, token: &str) -> String {
    [
        ("remote_url", remote_url),
        ("remote_branch", "main"),
        ("git_username", "x-access-token"),
        ("git_token", token),
        ("remote_push", "1"),
        ("export_mode", "terse"),
        ("host_key_policy", "accept-new"),
        ("schedule_hour", "3"),
        ("schedule_minute", "15"),
        ("concurrency", "4"),
        ("connect_timeout_secs", "10"),
        ("command_timeout_secs", "120"),
        ("path_template", "{tenant}/{device}.rsc"),
        ("committer_name", "DonDude"),
        ("committer_email", "dondude@example.org"),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", urlencode(value)))
    .collect::<Vec<_>>()
    .join("&")
}

fn device_body(name: &str, secret: &str) -> String {
    [
        ("name", name),
        ("host", "127.0.0.1"),
        ("port", "9"),
        ("username", "backup"),
        ("tenant", "acme"),
        ("tags", "core, milan"),
        ("auth_kind", "password"),
        ("secret", secret),
        ("private_key_path", ""),
        ("enabled", "1"),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", urlencode(value)))
    .collect::<Vec<_>>()
    .join("&")
}

/// Minimal percent-encoding for the characters these bodies actually contain.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

struct Client {
    state: AppState,
    cookie: Option<String>,
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

impl Client {
    async fn send(&mut self, method: &str, uri: &str, body: Option<String>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let mut request = request
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();

        // The login throttle reads the peer address, which `oneshot` does not
        // supply on its own.
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4242))));

        let response = router(self.state.clone())
            .oneshot(request)
            .await
            .expect("router must respond");

        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(set) = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
        {
            self.cookie = Some(set.to_string());
        }
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("body must be readable");
        Reply {
            status,
            location,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&mut self, uri: &str) -> Reply {
        self.send("GET", uri, None).await
    }

    async fn post(&mut self, uri: &str, body: String) -> Reply {
        self.send("POST", uri, Some(body)).await
    }
}

#[tokio::test]
async fn the_web_interface_round_trips_what_an_operator_submits() {
    let Ok(dsn) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping the web interface test");
        return;
    };

    let key = MasterKey::generate().unwrap();
    let db = Arc::new(
        Db::connect(&dsn, 4, MasterKey::from_base64(&key).unwrap())
            .await
            .expect("connect"),
    );
    db.migrate().await.expect("migrate");
    sqlx::query(
        "TRUNCATE users, sessions, tenants, devices, backup_runs, backup_events,
             login_attempts CASCADE",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE settings SET remote_url = NULL, git_token_sealed = NULL WHERE id")
        .execute(db.pool())
        .await
        .unwrap();

    let repo = tempfile::tempdir().unwrap();
    let mut client = Client {
        state: AppState::new(Arc::clone(&db), repo.path().to_path_buf()),
        cookie: None,
    };

    // --- authentication ----------------------------------------------------
    let reply = client.get("/").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location.as_deref(), Some("/login"));

    // With no accounts, the login page points at setup.
    let reply = client.get("/login").await;
    assert_eq!(reply.location.as_deref(), Some("/setup"));

    let reply = client
        .post(
            "/setup",
            "username=admin&password=supersecret1&confirm=supersecret1".to_string(),
        )
        .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/"),
        "setup must sign us in"
    );

    let reply = client.get("/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Dashboard"));
    // The build has to be identifiable without shelling into the container.
    assert!(
        reply.body.contains(mikrotik_dondude::VERSION),
        "the version must be on the page"
    );

    // --- settings: the regression this test exists for ---------------------
    assert!(
        reply.body.contains("No GitHub repository configured"),
        "the dashboard should nag while no remote is set"
    );

    // "Save and test" must persist even though the connection test fails: the
    // old behaviour re-rendered from stored settings and silently dropped the
    // URL the operator had just typed.
    let reply = client
        .post(
            "/settings/test",
            settings_body(REPO_URL, "github_pat_NotARealToken"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains(REPO_URL),
        "the submitted URL must still be in the form"
    );
    assert!(
        reply.body.contains("Settings saved"),
        "the operator must be told it was stored: {}",
        first_banner(&reply.body)
    );
    assert!(
        !reply.body.contains("NotARealToken"),
        "the token must never be echoed back into the page"
    );

    let settings = db.settings().await.unwrap();
    assert_eq!(settings.remote_url.as_deref(), Some(REPO_URL));
    assert!(settings.has_git_token);

    let reply = client.get("/").await;
    assert!(
        !reply.body.contains("No GitHub repository configured"),
        "the dashboard must stop nagging once a remote is set"
    );

    // Saving again with an empty token keeps the stored one.
    let reply = client.post("/settings", settings_body(REPO_URL, "")).await;
    assert_eq!(reply.location.as_deref(), Some("/settings?ok=settings"));
    assert!(db.settings().await.unwrap().has_git_token);

    // --- devices -----------------------------------------------------------
    let reply = client
        .post("/devices", device_body("core-rtr-01", "router-secret"))
        .await;
    assert_eq!(reply.location.as_deref(), Some("/devices?ok=created"));

    let reply = client.get("/devices").await;
    assert!(reply.body.contains("core-rtr-01"));
    assert!(
        !reply.body.contains("router-secret"),
        "a stored password must never reach the page"
    );

    // A rejected create must come back with the values still filled in, and must
    // still post to /devices rather than to an update of a nonexistent device.
    let reply = client
        .post("/devices", device_body("core-rtr-01", "another"))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("already exists in this tenant"));
    assert!(reply.body.contains("value=\"core-rtr-01\""));
    assert!(
        reply.body.contains("action=\"/devices\""),
        "the retry must post to the create endpoint"
    );

    // --- editing preserves the stored password -----------------------------
    let device = db.devices().await.unwrap().pop().unwrap();
    let reply = client
        .post(
            &format!("/devices/{}", device.id),
            device_body("core-rtr-01", ""),
        )
        .await;
    assert_eq!(reply.location.as_deref(), Some("/devices?ok=saved"));
    let config = db.runtime_config(repo.path().to_path_buf()).await.unwrap();
    match &config.devices[0].auth {
        mikrotik_dondude::config::DeviceAuth::Password(password) => {
            assert_eq!(
                password, "router-secret",
                "an empty field must keep the secret"
            )
        }
        other => panic!("expected password auth, got {other:?}"),
    }

    // --- signing out -------------------------------------------------------
    let reply = client.post("/logout", String::new()).await;
    assert_eq!(reply.location.as_deref(), Some("/login"));
    client.cookie = None;
    let reply = client.get("/devices").await;
    assert_eq!(reply.location.as_deref(), Some("/login"));

    // Leave nothing sealed with this run's disposable key.
    sqlx::query(
        "TRUNCATE users, sessions, tenants, devices, backup_runs, backup_events,
             login_attempts CASCADE",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE settings SET remote_url = NULL, git_token_sealed = NULL WHERE id")
        .execute(db.pool())
        .await
        .unwrap();
}

fn first_banner(body: &str) -> String {
    body.split("class=\"banner")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .unwrap_or("(no banner)")
        .to_string()
}
