//! Shared guard for the tests that need a live PostgreSQL.
//!
//! Those tests `TRUNCATE` every table. Pointed at the wrong database they
//! silently destroy a working deployment — which is exactly what happened
//! during development, repeatedly, because the obvious DSN to hand them is the
//! one already in the shell's history.
//!
//! So they refuse to run unless the database name says it is scratch.

/// The DSN to test against, or `None` when no database was configured.
///
/// Panics — loudly, rather than skipping — if `TEST_DATABASE_URL` names a
/// database that does not look disposable. A typo here costs an operator their
/// inventory and their stored credentials, so it must not be survivable.
pub fn test_dsn() -> Option<String> {
    let dsn = std::env::var("TEST_DATABASE_URL").ok()?;
    let name = database_name(&dsn);

    assert!(
        name.contains("test"),
        "refusing to run: TEST_DATABASE_URL points at database `{name}`, which does not look \
         like a scratch database. These tests TRUNCATE every table. Create one and point at \
         it instead:\n\n    \
         createdb dondude_test   # or: docker exec <pg> psql -U dondude -d postgres \
         -c 'CREATE DATABASE dondude_test'\n    \
         TEST_DATABASE_URL=postgres://user:pass@host:port/dondude_test cargo test\n"
    );
    Some(dsn)
}

/// The database name from a PostgreSQL DSN: the last path segment, without any
/// query string. Falls back to the whole DSN so a malformed one fails the check
/// rather than passing it.
fn database_name(dsn: &str) -> &str {
    let without_query = dsn.split(['?', '#']).next().unwrap_or(dsn);
    without_query
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(dsn)
}

#[cfg(test)]
mod tests {
    use super::database_name;

    #[test]
    fn database_name_is_extracted_from_a_dsn() {
        assert_eq!(
            database_name("postgres://u:p@host:5432/dondude_test"),
            "dondude_test"
        );
        assert_eq!(
            database_name("postgres://u:p@host:5432/dondude_test?sslmode=require"),
            "dondude_test"
        );
        assert_eq!(database_name("postgres://u:p@host/dondude"), "dondude");
        // No name at all must fail the "looks like a test database" check.
        assert!(!database_name("postgres://u:p@host/").contains("test"));
        assert!(!database_name("nonsense").contains("test"));
    }
}
