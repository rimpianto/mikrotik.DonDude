//! CLI behaviour that needs no database.
//!
//! The point of these is the *first* five minutes: a new operator runs the
//! binary with nothing configured, and the message they get has to tell them
//! what to do next.

use std::process::{Command, Output};

use base64::Engine;

fn dondude(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dondude"))
        .args(args)
        // Never inherit a developer's real deployment.
        .env_remove("DATABASE_URL")
        .env_remove("DONDUDE_MASTER_KEY")
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run dondude")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn keygen_prints_a_usable_master_key_without_any_configuration() {
    let output = dondude(&["keygen"]);
    assert!(output.status.success(), "{}", combined(&output));

    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&key)
        .expect("keygen must print base64");
    assert_eq!(raw.len(), 32, "expected a 256-bit key");

    // Two invocations must not agree.
    let second = dondude(&["keygen"]);
    assert_ne!(key, String::from_utf8_lossy(&second.stdout).trim());

    // And it must say what the key is for.
    assert!(combined(&output).contains("DONDUDE_MASTER_KEY"));
}

#[test]
fn a_missing_database_url_says_so_with_an_example() {
    let output = dondude(&["db", "check"]);
    assert!(!output.status.success());
    let text = combined(&output);
    assert!(text.contains("DATABASE_URL"), "{text}");
    assert!(text.contains("postgres://"), "no example given: {text}");
}

#[test]
fn a_missing_master_key_points_at_keygen() {
    let output = Command::new(env!("CARGO_BIN_EXE_dondude"))
        .args(["db", "check"])
        .env("DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")
        .env_remove("DONDUDE_MASTER_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = combined(&output);
    assert!(text.contains("DONDUDE_MASTER_KEY"), "{text}");
    assert!(text.contains("keygen"), "no remedy offered: {text}");
}

#[test]
fn an_unparsable_master_key_is_rejected_before_connecting() {
    let output = Command::new(env!("CARGO_BIN_EXE_dondude"))
        .args(["db", "check"])
        // Valid base64, wrong length: the mistake to catch is a truncated paste.
        .env("DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")
        .env("DONDUDE_MASTER_KEY", "c2hvcnQ=")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        combined(&output).contains("32 bytes"),
        "{}",
        combined(&output)
    );
}

#[test]
fn help_lists_serve_as_the_way_in() {
    let output = dondude(&["--help"]);
    assert!(output.status.success());
    let text = combined(&output);
    for expected in ["serve", "backup", "device", "fleet", "user", "keygen"] {
        assert!(
            text.contains(expected),
            "`{expected}` missing from help: {text}"
        );
    }
}
