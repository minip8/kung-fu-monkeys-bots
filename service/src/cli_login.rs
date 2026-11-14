//! The `kfm` command line tool's login (`docs/ladder-api.md` §5.1, API v1.3):
//! the local-redirect flow of `gh auth login`. The CLI listens on an
//! ephemeral `127.0.0.1` port and opens `GET /auth/cli?port=&state=&host=`;
//! a signed-in person confirms on that page (a form with the session's CSRF
//! token), which creates a `submit` token named `kfm on <host>` and redirects
//! the browser to `http://127.0.0.1:<port>/callback?state=<state>&code=<code>`;
//! the CLI exchanges the code with `POST /auth/cli/exchange` (no auth) for the
//! token's secret, so the secret never rides in a browser URL.
//!
//! The code is 128 random bits, single use, valid for [`CODE_TTL_SECONDS`],
//! stored as its SHA-256 and bound to the token it unlocks: the row holds the
//! token's id and its secret sealed under the code (XOR with a SHA-512 of the
//! code, domain separated from the row's key), and the exchange checks the
//! unsealed secret against the token's own stored hash.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::header::LOCATION;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha512};
use uuid::Uuid;

use crate::auth::MaybeSession;
use crate::crypto;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::extract::{FormBody, JsonBody};
use crate::oauth::Provider;
use crate::portal;
use crate::tokens::{self, Scope};
use crate::{users, AppState};

/// The confirmation page's path and query start: the only page a sign-in
/// returns to other than `/` (`oauth::safe_next`).
pub const PAGE_PREFIX: &str = "/auth/cli?";
/// How long a code may wait for its exchange.
pub const CODE_TTL_SECONDS: i32 = 300;
/// The lowest local port the CLI may name (ephemeral ports are far above it).
pub const MIN_PORT: u16 = 1024;
/// A `state` is 8 to 128 characters of the URL-safe alphabet.
pub const MIN_STATE: usize = 8;
pub const MAX_STATE: usize = 128;
/// The token's name without a host.
pub const DEFAULT_NAME: &str = "kfm CLI";
const NAME_PREFIX: &str = "kfm on ";
const SEAL_PURPOSE: &[u8] = b"kfm-cli-code-seal\0";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/cli", get(page).post(confirm))
        .route("/auth/cli/exchange", post(exchange))
}

/// What the CLI asked for, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub port: u16,
    pub state: String,
    /// The host name as given, cleaned; empty when none.
    pub host: String,
}

impl Request {
    /// The token's name: `kfm on <host>`, at most 64 characters.
    pub fn token_name(&self) -> String {
        if self.host.is_empty() {
            return DEFAULT_NAME.to_owned();
        }
        let room = tokens::MAX_NAME_CHARS - NAME_PREFIX.chars().count();
        format!(
            "{NAME_PREFIX}{}",
            self.host.chars().take(room).collect::<String>()
        )
    }

    /// The page's own URL (what a sign-in comes back to).
    pub fn page_url(&self) -> String {
        let mut url = format!("{PAGE_PREFIX}port={}&state={}", self.port, self.state);
        if !self.host.is_empty() {
            url.push_str("&host=");
            url.extend(url::form_urlencoded::byte_serialize(self.host.as_bytes()));
        }
        url
    }

    /// The CLI's listener with `pairs` after the state.
    pub fn callback(&self, pairs: &[(&str, &str)]) -> String {
        let mut url = format!(
            "http://127.0.0.1:{}/callback?state={}",
            self.port, self.state
        );
        for (k, v) in pairs {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.extend(url::form_urlencoded::byte_serialize(v.as_bytes()));
        }
        url
    }
}

/// The CLI's parameters: `port` a number from [`MIN_PORT`], `state` 8 to 128
/// characters of `A-Z a-z 0-9 - _ . ~`, `host` optional (control characters
/// dropped, whitespace collapsed); `None` names what is wrong.
pub fn parse_request(
    port: Option<&str>,
    state: Option<&str>,
    host: Option<&str>,
) -> Result<Request, &'static str> {
    let port = port
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|&p| p >= MIN_PORT)
        .ok_or("the port is missing or not a port number from 1024")?;
    let state = state.unwrap_or_default();
    let state_ok = (MIN_STATE..=MAX_STATE).contains(&state.len())
        && state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b));
    if !state_ok {
        return Err("the state is missing or malformed");
    }
    let host = host
        .map(|h| {
            h.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .filter(|c| !c.is_control())
                .take(tokens::MAX_NAME_CHARS)
                .collect::<String>()
        })
        .unwrap_or_default();
    Ok(Request {
        port,
        state: state.to_owned(),
        host,
    })
}

/// The pad that seals a secret of up to 64 bytes under `code`.
fn pad(code: &[u8]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    hasher.update(SEAL_PURPOSE);
    hasher.update(code);
    hasher.finalize().into()
}

/// `secret` sealed (or unsealed: XOR is its own inverse) under `code`.
pub fn seal(code: &[u8], secret: &[u8]) -> Vec<u8> {
    debug_assert!(secret.len() <= 64);
    secret.iter().zip(pad(code)).map(|(s, p)| s ^ p).collect()
}

struct ProviderLink {
    label: &'static str,
    href: String,
}

#[derive(Template)]
#[template(path = "cli.html")]
struct CliPage {
    error: Option<String>,
    /// Signed out: the sign-in links that come back here.
    sign_in: Vec<ProviderLink>,
    /// Signed in: the confirmation.
    confirm: Option<ConfirmView>,
}

struct ConfirmView {
    user: String,
    token_name: String,
    csrf: String,
    port: u16,
    state: String,
    host: String,
}

/// The CLI page may redirect its form to the CLI's listener, so its
/// `form-action` names `http://127.0.0.1:*` beside `'self'` (browsers apply
/// `form-action` to a form's redirects too).
const CLI_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; \
     form-action 'self' http://127.0.0.1:*; frame-ancestors 'none'; base-uri 'none'";

fn render(status: StatusCode, page: CliPage) -> AppResult<Response> {
    let html = page.render().map_err(AppError::internal)?;
    Ok(portal::page_response_csp(status, html, CLI_CSP))
}

fn error_page(message: &str) -> AppResult<Response> {
    render(
        StatusCode::BAD_REQUEST,
        CliPage {
            error: Some(format!(
                "This login link is not valid: {message}. Run kfm login again."
            )),
            sign_in: Vec::new(),
            confirm: None,
        },
    )
}

#[derive(Deserialize)]
struct PageQuery {
    port: Option<String>,
    state: Option<String>,
    host: Option<String>,
}

/// `GET /auth/cli?port=&state=&host=`: signed out, the sign-in links (each
/// returning here); signed in, the confirmation form.
async fn page(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> AppResult<Response> {
    let Ok(Query(q)) = query else {
        return error_page("the query is malformed");
    };
    let request = match parse_request(q.port.as_deref(), q.state.as_deref(), q.host.as_deref()) {
        Ok(request) => request,
        Err(message) => return error_page(message),
    };
    let Some(session) = session else {
        let next: String =
            url::form_urlencoded::byte_serialize(request.page_url().as_bytes()).collect();
        return render(
            StatusCode::OK,
            CliPage {
                error: None,
                sign_in: Provider::ALL
                    .iter()
                    .map(|p| ProviderLink {
                        label: p.label(),
                        href: format!("/auth/{}?next={next}", p.as_str()),
                    })
                    .collect(),
                confirm: None,
            },
        );
    };
    let user = users::get_user(&state.db, session.user_id).await?;
    render(
        StatusCode::OK,
        CliPage {
            error: None,
            sign_in: Vec::new(),
            confirm: Some(ConfirmView {
                user: user.display_name,
                token_name: request.token_name(),
                csrf: session.csrf.clone(),
                port: request.port,
                state: request.state.clone(),
                host: request.host.clone(),
            }),
        },
    )
}

#[derive(Deserialize)]
struct ConfirmForm {
    #[serde(default)]
    csrf: String,
    port: Option<String>,
    state: Option<String>,
    host: Option<String>,
    /// `allow` creates the token; anything else cancels.
    #[serde(default)]
    decision: String,
}

fn see_other(location: String) -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, location)]).into_response()
}

/// Removes the codes past their time and revokes the tokens nobody collected.
async fn sweep(db: &sqlx::PgPool) -> AppResult<()> {
    sqlx::query(
        "WITH gone AS (DELETE FROM cli_codes WHERE expires_at <= now() RETURNING token_id) \
         UPDATE tokens SET revoked_at = now() \
         WHERE id IN (SELECT token_id FROM gone) AND revoked_at IS NULL",
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Creates the token and its code; the code as sent to the CLI.
async fn issue(state: &AppState, user_id: Uuid, request: &Request) -> AppResult<String> {
    sweep(&state.db).await?;
    let mut tx = state.db.begin().await?;
    let (info, secret) =
        tokens::create_token_in(&mut tx, user_id, &request.token_name(), Scope::Submit).await?;
    let code: [u8; 16] = crypto::random();
    sqlx::query(
        "INSERT INTO cli_codes (hash, token_id, sealed, expires_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(&crypto::sha256(&code)[..])
    .bind(info.id)
    .bind(seal(&code, secret.as_bytes()))
    .bind(f64::from(CODE_TTL_SECONDS))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(user = %user_id, token = %info.id, "CLI login confirmed");
    Ok(crypto::b64(&code))
}

/// `POST /auth/cli` (portal form: `csrf`, `port`, `state`, `host`,
/// `decision`). Without a session or with a bad CSRF token: back to the
/// portal with the error, nothing created. Otherwise a `303` to the CLI's
/// listener: `&code=` on `allow`, `&error=access_denied` on a cancel, and
/// `&error=<code>` when the token could not be created (`limit_reached`).
async fn confirm(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    FormBody(form): FormBody<ConfirmForm>,
) -> Response {
    let Some(session) = session else {
        return see_other(format!("/?error={}", ErrorCode::Unauthenticated.as_str()));
    };
    if let Err(err) = session.check_csrf(Some(&form.csrf)) {
        return see_other(format!("/?error={}", err.code().as_str()));
    }
    let request = match parse_request(
        form.port.as_deref(),
        form.state.as_deref(),
        form.host.as_deref(),
    ) {
        Ok(request) => request,
        Err(_) => return see_other(format!("/?error={}", ErrorCode::BadRequest.as_str())),
    };
    if form.decision != "allow" {
        return see_other(request.callback(&[("error", "access_denied")]));
    }
    match issue(&state, session.user_id, &request).await {
        Ok(code) => see_other(request.callback(&[("code", &code)])),
        Err(err) => see_other(request.callback(&[("error", err.code().as_str())])),
    }
}

#[derive(Deserialize)]
pub struct ExchangeRequest {
    pub code: String,
}

/// `POST /auth/cli/exchange {"code"}` (auth: none): the token the code
/// unlocks, once. `invalid_code` for a code that is malformed, unknown,
/// already used or expired (an expired one's token is revoked).
async fn exchange(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<ExchangeRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let code = crypto::unb64(body.code.trim())
        .filter(|c| c.len() == 16)
        .ok_or(AppError(ErrorCode::InvalidCode))?;
    let mut tx = state.db.begin().await?;
    let row: Option<(Uuid, Vec<u8>, bool)> = sqlx::query_as(
        "DELETE FROM cli_codes WHERE hash = $1 \
         RETURNING token_id, sealed, expires_at > now()",
    )
    .bind(&crypto::sha256(&code)[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some((token_id, sealed, live)) = row else {
        return Err(AppError(ErrorCode::InvalidCode));
    };
    if !live {
        sqlx::query("UPDATE tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(token_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(AppError(ErrorCode::InvalidCode));
    }
    let token: Option<(String, String, Vec<u8>)> =
        sqlx::query_as("SELECT name, scope, hash FROM tokens WHERE id = $1 AND revoked_at IS NULL")
            .bind(token_id)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let secret = seal(&code, &sealed);
    let (name, scope, hash) = token.ok_or(AppError(ErrorCode::InvalidCode))?;
    if crypto::sha256(&secret)[..] != hash[..] {
        return Err(AppError::internal("a CLI code unsealed to another secret"));
    }
    let secret = String::from_utf8(secret).map_err(AppError::internal)?;
    Ok(Json(serde_json::json!({
        "token": secret,
        "token_id": token_id,
        "name": name,
        "scope": scope,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_checked() {
        let ok = parse_request(
            Some("49152"),
            Some("abcdEFGH-_.~1234"),
            Some("  my \t laptop\n"),
        )
        .unwrap();
        assert_eq!(ok.port, 49152);
        assert_eq!(ok.host, "my laptop");
        assert_eq!(ok.token_name(), "kfm on my laptop");
        assert!(parse_request(Some("80"), Some("abcdefgh"), None).is_err());
        assert!(parse_request(Some("70000"), Some("abcdefgh"), None).is_err());
        assert!(parse_request(None, Some("abcdefgh"), None).is_err());
        assert!(parse_request(Some("5000"), Some("short"), None).is_err());
        assert!(parse_request(Some("5000"), Some("abc/defgh"), None).is_err());
        assert!(parse_request(Some("5000"), Some(&"a".repeat(129)), None).is_err());
        assert!(parse_request(Some("5000"), None, None).is_err());
        let bare = parse_request(Some("5000"), Some("abcdefgh"), None).unwrap();
        assert_eq!(bare.token_name(), DEFAULT_NAME);
    }

    #[test]
    fn names_fit_the_token_rule() {
        let long = parse_request(Some("5000"), Some("abcdefgh"), Some(&"h".repeat(200))).unwrap();
        let name = long.token_name();
        assert_eq!(name.chars().count(), tokens::MAX_NAME_CHARS);
        assert!(tokens::clean_name(&name).is_ok());
    }

    #[test]
    fn urls_carry_the_state_and_escape_the_rest() {
        let r = parse_request(Some("5000"), Some("abcdefgh"), Some("a&b c")).unwrap();
        assert_eq!(
            r.page_url(),
            "/auth/cli?port=5000&state=abcdefgh&host=a%26b+c"
        );
        assert_eq!(
            r.callback(&[("code", "x/y")]),
            "http://127.0.0.1:5000/callback?state=abcdefgh&code=x%2Fy"
        );
        assert!(crate::oauth::safe_next(Some(&r.page_url())).is_some());
    }

    #[test]
    fn sealing_round_trips_and_depends_on_the_code() {
        let secret = b"kfm_0123456789012345678901234567890123456789012";
        let sealed = seal(b"code-one-16bytes", secret);
        assert_ne!(&sealed[..], &secret[..]);
        assert_eq!(seal(b"code-one-16bytes", &sealed), secret.to_vec());
        assert_ne!(seal(b"code-two-16bytes", &sealed), secret.to_vec());
    }

    #[test]
    fn only_the_cli_page_is_a_next() {
        use crate::oauth::safe_next;
        assert!(safe_next(Some("/auth/cli?port=5000&state=abcdefgh")).is_some());
        assert!(safe_next(Some("/")).is_none());
        assert!(safe_next(Some("https://evil.example/auth/cli?")).is_none());
        assert!(safe_next(Some("//evil.example/auth/cli?")).is_none());
        assert!(safe_next(Some("/auth/cli?x=//evil")).is_none());
        assert!(safe_next(Some("/auth/cli?x=a b")).is_none());
        assert!(safe_next(Some("/auth/cli?x=\\")).is_none());
        assert!(safe_next(None).is_none());
    }
}
