//! Login sessions.
//!
//! A session is a 256-bit random token in an `HttpOnly` cookie; only its digest
//! is stored (see [`crate::crypto`]). `SameSite=Lax` is what protects the
//! state-changing forms: a cross-site POST arrives without the cookie, so no
//! per-form CSRF token is needed for same-origin forms.

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};

use crate::db::User;
use crate::web::AppState;

pub const COOKIE_NAME: &str = "dondude_session";

/// An authenticated operator. Extracting it is what makes a route private.
#[derive(Debug, Clone)]
pub struct Operator(pub User);

/// Redirect to the login page instead of returning a bare 401, so a browser
/// landing on a private URL gets somewhere useful.
pub struct LoginRequired;

impl IntoResponse for LoginRequired {
    fn into_response(self) -> Response {
        Redirect::to("/login").into_response()
    }
}

impl FromRequestParts<AppState> for Operator {
    type Rejection = LoginRequired;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = session_cookie(parts).ok_or(LoginRequired)?;
        match state.db.session_user(&token).await {
            Ok(Some(user)) => Ok(Operator(user)),
            Ok(None) => Err(LoginRequired),
            Err(error) => {
                // A database outage must not look like a bad password.
                tracing::error!(%error, "could not look up the session");
                Err(LoginRequired)
            }
        }
    }
}

/// For pages that render differently when signed in but do not require it.
impl OptionalFromRequestParts<AppState> for Operator {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <Operator as FromRequestParts<AppState>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

/// Read our cookie out of the `Cookie` header.
pub fn session_cookie(parts: &Parts) -> Option<String> {
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    cookie_value(header, COOKIE_NAME)
}

/// Pull one cookie out of a `Cookie` header value.
///
/// Hand-rolled rather than pulled from a crate: the header is a list of
/// `name=value` pairs separated by `; `, and a session cookie has no attributes
/// to parse on the way in.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// `Set-Cookie` value that installs a session.
///
/// `Secure` is deliberately *not* set: DonDude is commonly reached over plain
/// HTTP on a management LAN, and a `Secure` cookie would silently never be sent,
/// making login appear broken. Put a TLS-terminating proxy in front for
/// internet-facing deployments.
pub fn set_cookie(token: &str, ttl_days: i64) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        ttl_days * 24 * 60 * 60
    )
}

/// `Set-Cookie` value that clears the session.
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_parsed_out_of_a_combined_header() {
        let header = "theme=dark; dondude_session=abc123; other=x";
        assert_eq!(cookie_value(header, COOKIE_NAME).as_deref(), Some("abc123"));
        assert_eq!(cookie_value(header, "theme").as_deref(), Some("dark"));
        assert_eq!(cookie_value(header, "absent"), None);
        // A prefix must not match a different cookie name.
        assert_eq!(cookie_value("xdondude_session=no", COOKIE_NAME), None);
        assert_eq!(cookie_value("", COOKIE_NAME), None);
        assert_eq!(cookie_value("malformed", COOKIE_NAME), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_and_same_site() {
        let header = set_cookie("token", 30);
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Max-Age=2592000"));
        assert!(clear_cookie().contains("Max-Age=0"));
    }
}
