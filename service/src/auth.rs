//! Who is calling: a portal session (a signed cookie) or an API token (a
//! bearer), and the CSRF rule for session writes (`docs/ladder-api.md` §3).

use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use sqlx::PgPool;
use uuid::Uuid;

use crate::crypto;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::tokens::{self, Scope};
use crate::AppState;

pub const SESSION_COOKIE: &str = "kfm_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// A session ends after this long without a request.
pub const SESSION_IDLE_DAYS: i32 = 30;
/// The cookie outlives any session (400 days, the browsers' cap); the row's
/// `last_seen_at` is what expires a session.
const SESSION_COOKIE_MAX_AGE: u64 = 400 * 24 * 3600;
const SESSION_PURPOSE: &str = "kfm-session";

/// A signed-in portal session.
#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: Uuid,
    /// SHA-256 of the session's random id: the `sessions` row's key.
    pub key: [u8; 32],
    /// The session's CSRF token, carried by every portal form and by the
    /// `X-CSRF-Token` header of a session-authenticated JSON write.
    pub csrf: String,
}

impl Session {
    /// Checks a CSRF token presented with a write, in constant time.
    pub fn check_csrf(&self, presented: Option<&str>) -> AppResult<()> {
        use subtle::ConstantTimeEq;
        match presented {
            Some(p) if bool::from(p.as_bytes().ct_eq(self.csrf.as_bytes())) => Ok(()),
            _ => Err(AppError(ErrorCode::BadCsrf)),
        }
    }
}

/// How a request authenticated.
#[derive(Debug, Clone)]
pub enum Auth {
    Session(Session),
    Token { id: Uuid, scope: Scope },
}

/// An authenticated caller. A request with an `Authorization: Bearer` header
/// is judged by the token alone (an invalid one is `invalid_token` even if a
/// session cookie came too); otherwise by the session cookie; otherwise it is
/// `unauthenticated`.
#[derive(Debug, Clone)]
pub struct Caller {
    pub user_id: Uuid,
    pub auth: Auth,
}

impl Caller {
    /// Passes for a session (a person at the portal may do anything their
    /// account may) or a token whose scope covers `scope`.
    pub fn require_scope(&self, scope: Scope) -> AppResult<()> {
        match &self.auth {
            Auth::Session(_) => Ok(()),
            Auth::Token { scope: have, .. } if have.covers(scope) => Ok(()),
            Auth::Token { .. } => Err(AppError(ErrorCode::InsufficientScope)),
        }
    }

    /// The session, or `session_required` for a token.
    pub fn require_session(&self) -> AppResult<&Session> {
        match &self.auth {
            Auth::Session(session) => Ok(session),
            Auth::Token { .. } => Err(AppError(ErrorCode::SessionRequired)),
        }
    }

    /// A write that only the portal session may make: the session, its CSRF
    /// token checked against the `X-CSRF-Token` header.
    pub fn session_write(&self, headers: &HeaderMap) -> AppResult<&Session> {
        let session = self.require_session()?;
        session.check_csrf(headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()))?;
        Ok(session)
    }

    /// A write a session (with its CSRF header) or a token of `scope` may make.
    pub fn write_with_scope(&self, headers: &HeaderMap, scope: Scope) -> AppResult<()> {
        match &self.auth {
            Auth::Session(_) => self.session_write(headers).map(|_| ()),
            Auth::Token { .. } => self.require_scope(scope),
        }
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> AppResult<Caller> {
        if let Some(value) = parts.headers.get(AUTHORIZATION) {
            let secret = value
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or(AppError(ErrorCode::InvalidToken))?;
            let (user_id, id, scope) = tokens::resolve(&state.db, secret.trim())
                .await?
                .ok_or(AppError(ErrorCode::InvalidToken))?;
            return Ok(Caller {
                user_id,
                auth: Auth::Token { id, scope },
            });
        }
        match session_from_headers(state, &parts.headers).await? {
            Some(session) => Ok(Caller {
                user_id: session.user_id,
                auth: Auth::Session(session),
            }),
            None => Err(AppError(ErrorCode::Unauthenticated)),
        }
    }
}

/// The portal's view: the session cookie only (a bearer header is ignored),
/// `None` when signed out or expired.
pub struct MaybeSession(pub Option<Session>);

impl FromRequestParts<AppState> for MaybeSession {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> AppResult<MaybeSession> {
        Ok(MaybeSession(
            session_from_headers(state, &parts.headers).await?,
        ))
    }
}

/// The value of cookie `name` in the request's `Cookie` headers.
pub fn cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// A `Set-Cookie` value: HttpOnly, SameSite=Lax, Secure when the public URL
/// is https.
pub fn set_cookie(name: &str, value: &str, path: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}")
}

async fn session_from_headers(state: &AppState, headers: &HeaderMap) -> AppResult<Option<Session>> {
    let Some(value) = cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let Some(id) = crypto::verify(&state.config.session_key, SESSION_PURPOSE, value) else {
        return Ok(None);
    };
    let key = crypto::sha256(&id);
    // One statement checks the idle limit and records this request.
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "UPDATE sessions SET last_seen_at = now() \
         WHERE id = $1 AND last_seen_at > now() - make_interval(days => $2) \
         RETURNING user_id, csrf",
    )
    .bind(&key[..])
    .bind(SESSION_IDLE_DAYS)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(user_id, csrf)| Session { user_id, key, csrf }))
}

/// Starts a session for `user_id`; returns its `Set-Cookie` value.
pub async fn start_session(state: &AppState, user_id: Uuid) -> AppResult<String> {
    // Expired rows go when anyone signs in; the index makes it cheap.
    sqlx::query("DELETE FROM sessions WHERE last_seen_at < now() - make_interval(days => $1)")
        .bind(SESSION_IDLE_DAYS)
        .execute(&state.db)
        .await?;
    let id: [u8; 16] = crypto::random();
    let csrf = crypto::b64(&crypto::random::<16>());
    sqlx::query("INSERT INTO sessions (id, user_id, csrf) VALUES ($1, $2, $3)")
        .bind(&crypto::sha256(&id)[..])
        .bind(user_id)
        .bind(&csrf)
        .execute(&state.db)
        .await?;
    let value = crypto::sign(&state.config.session_key, SESSION_PURPOSE, &id);
    Ok(set_cookie(
        SESSION_COOKIE,
        &value,
        "/",
        SESSION_COOKIE_MAX_AGE,
        state.config.secure_cookies(),
    ))
}

/// Ends a session; returns the `Set-Cookie` value that clears the cookie.
pub async fn end_session(db: &PgPool, session: &Session, secure: bool) -> AppResult<String> {
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(&session.key[..])
        .execute(db)
        .await?;
    Ok(clear_session_cookie(secure))
}

pub fn clear_session_cookie(secure: bool) -> String {
    set_cookie(SESSION_COOKIE, "", "/", 0, secure)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn cookies_are_found_across_headers() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("a=1; kfm_session=x.y"));
        headers.append(COOKIE, HeaderValue::from_static("b=2"));
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("x.y"));
        assert_eq!(cookie(&headers, "b"), Some("2"));
        assert_eq!(cookie(&headers, "c"), None);
    }

    #[test]
    fn csrf_must_match() {
        let session = Session {
            user_id: Uuid::nil(),
            key: [0; 32],
            csrf: "abc".to_owned(),
        };
        assert!(session.check_csrf(Some("abc")).is_ok());
        assert!(session.check_csrf(Some("abd")).is_err());
        assert!(session.check_csrf(None).is_err());
    }
}
