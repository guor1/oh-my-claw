//! Bearer-token middleware for the native and OpenAI API routes.
//!
//! Policy (方案 A):
//! - When no token is configured, the server must be bound to loopback only.
//!   Any address other than 127.0.0.1 or ::1 requires a token at startup.
//! - When a token is configured, every request to `/api/v1/*` and `/v1/*`
//!   must carry `Authorization: Bearer <token>` (or `?token=`, for the
//!   ambient `EventSource` stream only). `/health` and the static assets under
//!   `/` are exempt so the browser can reach the page first.
//!
//! The bind-address-vs-token rule is checked by [`check_bind_requires_token`],
//! which the CLI calls before binding the socket, so a misconfiguration fails
//! before any traffic arrives.

use axum::{
    extract::{Request, State as AxumState},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use crate::server::AppState;

/// Name of the HttpOnly cookie that carries an established login session.
const LOGIN_COOKIE: &str = "oc_session";
/// The only route that accepts `?token=` as a fallback: `EventSource` cannot
/// set custom headers, so the ambient-event stream carries the token in the
/// query string. Everywhere else the token must come from the header, keeping
/// it out of access logs, Referer headers and browser history.
const TOKEN_QUERY_ROUTE: &str = "/api/v1/events";
/// Lifetime of a login session cookie before the operator must re-authenticate.
pub const LOGIN_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Middleware: allow a request through only if it carries a valid credential.
///
/// Accepts, in order: an `Authorization: Bearer <token>` header (machine
/// clients), an `oc_session` HttpOnly cookie (the Web UI after login), or a
/// `?token=` query param — but only on [`TOKEN_QUERY_ROUTE`], for `EventSource`.
///
/// Routes that do not start with `/api/v1`, `/v1` or [`LOGIN_ROUTE`] (i.e.
/// `/health` and static assets) are passed through unconditionally so the
/// browser can load the UI before it knows the token.
pub async fn require_token(
    AxumState(state): AxumState<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = &state.token else {
        return next.run(req).await;
    };

    let path = req.uri().path();
    // `/auth/login` (not under /api/v1 or /v1) is exempt here — the login
    // handler performs its own constant-time token check before setting a cookie.
    if !path.starts_with("/api/v1") && !path.starts_with("/v1") {
        return next.run(req).await;
    }

    // 1. Authorization header (most clients)
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // 2. HttpOnly login cookie (the Web UI, after /auth/login).
    let cookie_token = cookie_session(req.headers(), &state).await;

    // 3. ?token= query param — only for the ambient EventSource stream.
    //    URL-decoded (`form_urlencoded`), so base64/`+`/`/`-bearing tokens work.
    let query_token = (path == TOKEN_QUERY_ROUTE)
        .then(|| req.uri().query())
        .flatten()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find_map(|(k, v)| if k == "token" { Some(v.into_owned()) } else { None })
        });

    // The cookie path already proves possession; the header/query still need to
    // be compared against the configured token.
    let ok = cookie_token
        || header_token
            .map(|t| bool::from(t.as_bytes().ct_eq(expected.as_bytes())))
            .unwrap_or(false)
        || query_token
            .as_deref()
            .map(|t| bool::from(t.as_bytes().ct_eq(expected.as_bytes())))
            .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "missing or invalid Authorization: Bearer token",
                    "type": "unauthorized"
                }
            })),
        )
            .into_response()
    }
}

/// Look up the `oc_session` cookie in the request and check it against the
/// in-memory login-session table (with expiry).
async fn cookie_session(headers: &axum::http::HeaderMap, state: &AppState) -> bool {
    let Some(sid) = extract_cookie(headers, LOGIN_COOKIE) else {
        return false;
    };
    let now = Instant::now();
    let valid = match state.login_sessions.get(&sid) {
        Some(created) => now.duration_since(*created) < state.login_ttl,
        None => false,
    };
    if !valid {
        // Expired or unknown: drop it so the table doesn't grow without bound.
        state.login_sessions.remove(&sid);
    }
    valid
}

/// `POST /auth/login` — exchange the static token for an HttpOnly session cookie.
///
/// The token is compared in constant time. On success a random session id is
/// stored (with [`LOGIN_TTL`]) and returned as a cookie; the token itself never
/// reaches the page markup, localStorage or the URL.
pub async fn login(
    AxumState(state): AxumState<AppState>,
    Json(body): Json<LoginReq>,
) -> Response {
    let Some(expected) = &state.token else {
        // No token configured: loopback-only no-auth mode. There is nothing to
        // log in against.
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": "no token configured", "type": "no_auth" } })),
        )
            .into_response();
    };

    let ok = bool::from(body.token.as_bytes().ct_eq(expected.as_bytes()));
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "message": "invalid token", "type": "unauthorized" } })),
        )
            .into_response();
    }

    let sid = uuid::Uuid::now_v7().to_string();
    state.login_sessions.insert(sid.clone(), Instant::now());

    let cookie = format!(
        "{LOGIN_COOKIE}={sid}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        state.login_ttl.as_secs()
    );

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct LoginReq {
    token: String,
}

/// Parse a single cookie by name out of the `Cookie` request header.
///
/// Returns the decoded value if present. `Cookie` values here are server-set
/// UUIDs, so no percent-decoding is needed; splitting on `;`/`=` is sufficient.
fn extract_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// Validate that a non-loopback bind address is paired with a non-empty token.
///
/// Call this during startup, before binding the socket. Returns an error
/// message suitable for printing to stderr; the caller should exit.
pub fn check_bind_requires_token(addr: &std::net::SocketAddr, token: &Option<String>) -> Result<(), String> {
    let ip = addr.ip();
    // `Some("")` (or a whitespace-only token) is treated as "no credential",
    // because `Authorization: Bearer ` with an empty credential would match it
    // and leave the daemon effectively unauthenticated on the network.
    let has_token = token.as_deref().is_some_and(|t| !t.trim().is_empty());
    if !ip.is_loopback() && !has_token {
        return Err(format!(
            "binding to {addr} exposes the daemon to the network; \
             supply a non-empty --token (or OC_HTTP_TOKEN) to enable non-loopback binding"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_needs_no_token() {
        assert!(check_bind_requires_token(&addr("127.0.0.1:8080"), &None).is_ok());
        assert!(check_bind_requires_token(&addr("[::1]:8080"), &None).is_ok());
    }

    #[test]
    fn non_loopback_requires_token() {
        assert!(check_bind_requires_token(&addr("0.0.0.0:8080"), &None).is_err());
        assert!(check_bind_requires_token(&addr("192.168.1.10:8080"), &None).is_err());
    }

    #[test]
    fn non_loopback_with_token_is_allowed() {
        assert!(check_bind_requires_token(&addr("0.0.0.0:8080"), &Some("s3cret".into())).is_ok());
    }

    #[test]
    fn non_loopback_empty_token_is_rejected() {
        // `--token ""` / `OC_HTTP_TOKEN=""` must not be treated as a configured
        // credential — an empty `Authorization: Bearer ` would match it.
        assert!(check_bind_requires_token(&addr("0.0.0.0:8080"), &Some("".into())).is_err());
        assert!(check_bind_requires_token(&addr("192.168.1.10:8080"), &Some("   ".into())).is_err());
    }

    #[test]
    fn loopback_empty_token_is_fine() {
        // On loopback there is no auth anyway; an empty token is harmless there.
        assert!(check_bind_requires_token(&addr("127.0.0.1:8080"), &Some("".into())).is_ok());
    }
}
