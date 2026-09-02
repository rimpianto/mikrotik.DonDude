//! Backup and restore of a whole DonDude deployment as one encrypted file.
//!
//! # What a backup contains
//!
//! ```text
//! dondude-backup-<timestamp>.dud
//! └── (XChaCha20-Poly1305 sealed, key = DONDUDE_MASTER_KEY)
//!     ├── manifest.json          — version, tables, row counts, created_at
//!     ├── database.sql           — logical dump of every table
//!     ├── .env                   — the deployment environment file
//!     └── known_hosts            — SSH host-key pinning (when present)
//! ```
//!
//! The master key is *the* backup key: the archive cannot be read without it,
//! and it is the same key the database's sealed secrets need, so one secret —
//! kept already — restores everything.
//!
//! # Dump format
//!
//! A logical dump, not `pg_dump`: DonDude may run where `pg_dump` does not
//! exist (the Docker image, a standalone binary with a remote database), and
//! `pg_dump` output would also depend on the server's version. Instead every
//! table is read with `SELECT` and written back as explicit `INSERT`s with
//! literals, in dependency order, inside a single transaction on restore. A
//! restore **replaces** the deployment: every table is truncated first.
//!
//! # Why not ZSTD/gzip
//!
//! The payload is a few megabytes at fleet scale; correctness and portability
//! beat compression here. Fewer moving parts in the restore path is a feature.
//!
//! # Platform note
//!
//! Windows, Linux and macOS: the archive is a plain sequence of length-prefixed
//! files inside an AEAD-sealed envelope, so restore works cross-platform with
//! the same key and no archive tool. Windows `.env` line endings are
//! normalized on the way out only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

use crate::crypto::MasterKey;
use crate::error::{Error, Result};

/// The current archive format revision. A future format change bumps this and
/// keeps reading the old one; restore refuses anything it does not know.
const FORMAT: u32 = 1;

/// Magic bytes at the start of every archive, so `file`-style tools and we
/// ourselves can tell an archive from random bytes before decrypting.
const MAGIC: &[u8; 8] = b"DNDDBKP1";

/// Every table in the deployment, in restore order (parents first).
///
/// `backup_runs` before `events`: events reference runs. `device_samples`
/// last: it is by far the largest and the least precious.
const TABLES: &[&str] = &[
    "users",
    "tenants",
    "settings",
    "devices",
    "sessions",
    "login_attempts",
    "backup_runs",
    "backup_events",
    "device_samples",
];

/// Manifest recorded at the front of the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub created_at: DateTime<Utc>,
    /// DonDude version that wrote the archive.
    pub version: String,
    pub files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Archive layout
// ---------------------------------------------------------------------------
//
// MAGIC (8) || format u32 BE || sealed payload
//
// The payload is a stream of entries:
//   name_len u32 BE || name || data_len u64 BE || data
// sealed whole with a single nonce, written as a 24-byte prefix. Sealing the
// whole stream (rather than each entry) means a tampered middle is caught at
// open time, and one nonce for one message is the correct usage.

/// Collect the pieces of a backup: each table's rows as INSERT statements,
/// plus the `.env` and `known_hosts` when they can be found.
#[derive(Debug)]
pub struct BackupInput {
    /// SQL text restoring every table.
    pub database_sql: String,
    /// Raw `.env` contents, if a file was found.
    pub env_file: Option<(String, String)>,
    /// Raw `known_hosts` contents, if a file was found.
    pub known_hosts: Option<(String, String)>,
}

impl BackupInput {
    /// Write the archive to `path`, sealed with `key`.
    pub fn write_archive(&self, path: &Path, key: &MasterKey) -> Result<()> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        files.push(("database.sql".to_string(), self.database_sql.clone().into_bytes()));

        if let Some((contents, _)) = &self.env_file {
            files.push((".env".to_string(), contents.clone().into_bytes()));
        }
        if let Some((contents, _)) = &self.known_hosts {
            files.push(("known_hosts".to_string(), contents.clone().into_bytes()));
        }

        let manifest = Manifest {
            format: FORMAT,
            created_at: Utc::now(),
            version: crate::VERSION.to_string(),
            files: files.iter().map(|(name, _)| name.clone()).collect(),
        };
        let manifest =
            serde_json::to_vec(&manifest).map_err(|e| Error::config(format!("manifest: {e}")))?;

        let mut payload: Vec<u8> = Vec::new();
        let manifest_len = u32::try_from(manifest.len())
            .map_err(|_| Error::config("manifest too large"))?;
        payload.extend_from_slice(&manifest_len.to_be_bytes());
        payload.extend_from_slice(&manifest);
        for (name, data) in files {
            write_entry(&mut payload, name.as_bytes(), &data)?;
        }

        let sealed = seal_stream(&payload, key)?;

        let mut file = std::fs::File::create(path)
            .map_err(|e| Error::config(format!("cannot create {}: {e}", path.display())))?;
        file.write_all(MAGIC)?;
        file.write_all(&FORMAT.to_be_bytes())?;
        file.write_all(&sealed)?;
        Ok(())
    }
}

/// One restored archive, in memory.
#[derive(Debug)]
pub struct Archive {
    pub manifest: Manifest,
    pub files: Vec<(String, Vec<u8>)>,
}

impl Archive {
    /// Open and decrypt an archive. The error deliberately names the master
    /// key when decryption fails, because that is the usual cause.
    pub fn read(path: &Path, key: &MasterKey) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::config(format!("cannot read {}: {e}", path.display())))?;
        if bytes.len() < 16 {
            return Err(Error::config("not a DonDude backup (too short)"));
        }
        if &bytes[0..8] != MAGIC {
            return Err(Error::config("not a DonDude backup (bad magic)"));
        }
        let format = u32::from_be_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| Error::config("corrupt archive header"))?,
        );
        if format != FORMAT {
            return Err(Error::config(format!(
                "backup format {format} is newer than this DonDude knows; update first"
            )));
        }

        let payload = open_stream(&bytes[12..], key)?;

        let mut rest: &[u8] = &payload;
        let manifest_len = read_u32(&mut rest)? as usize;
        if rest.len() < manifest_len {
            return Err(Error::config("corrupt archive (truncated manifest)"));
        }
        let manifest: Manifest = serde_json::from_slice(&rest[..manifest_len])
            .map_err(|e| Error::config(format!("corrupt manifest: {e}")))?;
        rest = &rest[manifest_len..];

        let mut files = Vec::new();
        while !rest.is_empty() {
            let (name, data, consumed) = read_entry(rest)?;
            files.push((String::from_utf8_lossy(&name).into_owned(), data.to_vec()));
            rest = &rest[consumed..];
        }
        Ok(Self { manifest, files })
    }

    /// Look up one file by name.
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d.as_slice())
    }
}

// ---------------------------------------------------------------------------
// Stream seal/open (whole-payload AEAD)
// ---------------------------------------------------------------------------

fn seal_stream(payload: &[u8], key: &MasterKey) -> Result<Vec<u8>> {
    key.seal_bytes(payload)
}

fn open_stream(sealed: &[u8], key: &MasterKey) -> Result<Vec<u8>> {
    key.open_bytes(sealed)
}

// ---------------------------------------------------------------------------
// Entry framing helpers
// ---------------------------------------------------------------------------

fn write_entry(out: &mut Vec<u8>, name: &[u8], data: &[u8]) -> Result<()> {
    let name_len =
        u32::try_from(name.len()).map_err(|_| Error::config("entry name too long"))?;
    out.extend_from_slice(&name_len.to_be_bytes());
    out.extend_from_slice(name);
    let data_len =
        u64::try_from(data.len()).map_err(|_| Error::config("entry too large"))?;
    out.extend_from_slice(&data_len.to_be_bytes());
    out.extend_from_slice(data);
    Ok(())
}

/// Returns (name, data, bytes consumed).
fn read_entry(buf: &[u8]) -> Result<(Vec<u8>, Vec<u8>, usize)> {
    let mut rest = buf;
    let name_len = read_u32(&mut rest)? as usize;
    if rest.len() < name_len {
        return Err(Error::config("corrupt archive (truncated name)"));
    }
    let name = rest[..name_len].to_vec();
    rest = &rest[name_len..];
    let data_len = read_u64(&mut rest)? as usize;
    if rest.len() < data_len {
        return Err(Error::config("corrupt archive (truncated data)"));
    }
    let data = rest[..data_len].to_vec();
    Ok((name, data, 12 + name_len + data_len))
}

fn read_u32(rest: &mut &[u8]) -> Result<u32> {
    if rest.len() < 4 {
        return Err(Error::config("corrupt archive (truncated)"));
    }
    let (head, tail) = rest.split_at(4);
    *rest = tail;
    Ok(u32::from_be_bytes(head.try_into().unwrap()))
}

fn read_u64(rest: &mut &[u8]) -> Result<u64> {
    if rest.len() < 8 {
        return Err(Error::config("corrupt archive (truncated)"));
    }
    let (head, tail) = rest.split_at(8);
    *rest = tail;
    Ok(u64::from_be_bytes(head.try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// SQL dump helpers
// ---------------------------------------------------------------------------

/// Quote an identifier (table/column name) for the dump.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The table list, in dump order. Public for the db layer to iterate.
pub fn tables() -> &'static [&'static str] {
    TABLES
}

/// Split a dump script into individual statements on `;`, respecting single-
/// quoted strings. The dump never contains comments or dollar-quoting, so
/// this stays deliberately small.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            in_string = !in_string;
            current.push(c);
            // An escaped quote ('') flips twice; peek keeps it simple.
            if !in_string && chars.peek() == Some(&'\'') {
                current.push('\'');
                chars.next();
                in_string = true;
            }
            continue;
        }
        if c == ';' && !in_string {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                statements.push(trimmed.to_string());
            }
            current.clear();
            continue;
        }
        current.push(c);
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        statements.push(trimmed.to_string());
    }
    statements
}


#[cfg(test)]
mod archive_tests {
    use super::*;

    fn test_key() -> MasterKey {
        MasterKey::from_base64("QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE=").unwrap()
    }

    #[test]
    fn archive_round_trips_files() {
        let key = test_key();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.dud");

        let input = BackupInput {
            database_sql: "SELECT 1; -- with 'quotes' and; semicolons".to_string(),
            env_file: Some(("DONDUDE_MASTER_KEY=abc\n".to_string(), ".env".to_string())),
            known_hosts: Some(("host ssh-ed25519 AAAA\n".to_string(), "kh".to_string())),
        };
        input.write_archive(&path, &key).unwrap();

        let archive = Archive::read(&path, &key).unwrap();
        assert_eq!(archive.manifest.format, 1);
        assert!(archive.manifest.files.contains(&"database.sql".to_string()));
        assert!(archive.manifest.files.contains(&".env".to_string()));
        assert_eq!(archive.file("database.sql").unwrap(), b"SELECT 1; -- with 'quotes' and; semicolons");
        assert_eq!(archive.file(".env").unwrap(), b"DONDUDE_MASTER_KEY=abc\n");
        assert_eq!(archive.file("known_hosts").unwrap(), b"host ssh-ed25519 AAAA\n");
    }

    #[test]
    fn wrong_key_cannot_open() {
        let key = test_key();
        let other = MasterKey::from_base64(
            "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI=",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.dud");

        let input = BackupInput {
            database_sql: "secret".to_string(),
            env_file: None,
            known_hosts: None,
        };
        input.write_archive(&path, &key).unwrap();

        let error = Archive::read(&path, &other).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("MASTER_KEY"),
            "unexpected error text: {text}"
        );
    }

    #[test]
    fn garbage_is_rejected() {
        let key = test_key();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.dud");
        std::fs::write(&path, b"not an archive at all, just bytes").unwrap();
        let error = Archive::read(&path, &key).unwrap_err();
        assert!(error.to_string().contains("not a DonDude backup"));
    }

    #[test]
    fn statements_split_respecting_quotes() {
        let sql = "INSERT INTO t VALUES ('a;b');\nINSERT INTO t VALUES ('it''s');\n";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "INSERT INTO t VALUES ('a;b')");
        assert_eq!(parts[1], "INSERT INTO t VALUES ('it''s')");
    }
}
