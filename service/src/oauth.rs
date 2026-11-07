//! Sign-in with GitHub (OAuth 2, the `oauth2` crate) and Google (OpenID
//! Connect with discovery, the `openidconnect` crate): the authorization code
//! flow with PKCE, its state in a signed short-lived cookie. An account is
//! keyed by the provider's subject id; an email is display data only.

use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::header::{CONTENT_TYPE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreResponseType};
use openidconnect::{AuthenticationFlow, IssuerUrl, Nonce};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{self, MaybeSession, Session, CSRF_HEADER};
use crate::crypto;
use crate::error::{is_unique_violation, AppError, AppResult, ErrorCode};
use crate::users::clean_display_name;
use crate::AppState;

pub const STATE_COOKIE: &str = "kfm_oauth";
const STATE_PURPOSE: &str = "kfm-oauth-state";
/// How long a sign-in may take between the start and the callback.
const STATE_TTL_SECONDS: i64 = 600;
/// How long Google's discovery document and keys are reused.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// A display name when the provider reports none.
const FALLBACK_NAME: &str = "player";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    Google,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::Google => "google",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub",
            Provider::Google => "Google",
        }
    }

    pub const ALL: [Provider; 2] = [Provider::GitHub, Provider::Google];
}

/// Google's discovery document (with its keys), reused for [`DISCOVERY_TTL`].
#[derive(Default)]
pub struct GoogleCache(tokio::sync::Mutex<Option<(Instant, CoreProviderMetadata)>>);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(start_github))
        .route("/auth/google", get(start_google))
        .route("/auth/github/callback", get(callback_github))
        .route("/auth/google/callback", get(callback_google))
        .route("/auth/logout", post(logout))
}

/// What travels in the state cookie between the start and the callback.
#[derive(Serialize, Deserialize)]
struct FlowState {
    /// Provider.
    p: String,
    /// The `state` parameter (CSRF of the redirect itself).
    s: String,
    /// The PKCE verifier.
    v: String,
    /// The OIDC nonce (Google).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<String>,
    /// Expiry, Unix seconds.
    e: i64,
}

/// What a provider says about the person signing in.
#[derive(Debug, Clone)]
pub struct Profile {
    pub provider: Provider,
    pub subject: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

fn redirect_uri(state: &AppState, provider: Provider) -> AppResult<RedirectUrl> {
    RedirectUrl::new(format!(
        "{}/auth/{}/callback",
        state.config.public_url,
        provider.as_str()
    ))
    .map_err(AppError::internal)
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn see_other(location: &str, cookies: Vec<String>) -> Response {
    let mut response = (StatusCode::SEE_OTHER, [(LOCATION, location.to_owned())]).into_response();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
}

fn state_cookie(state: &AppState, flow: &FlowState) -> AppResult<String> {
    let json = serde_json::to_vec(flow).map_err(AppError::internal)?;
    let value = crypto::sign(&state.config.session_key, STATE_PURPOSE, &json);
    Ok(auth::set_cookie(
        STATE_COOKIE,
        &value,
        "/auth",
        STATE_TTL_SECONDS.unsigned_abs(),
        state.config.secure_cookies(),
    ))
}

fn clear_state_cookie(state: &AppState) -> String {
    auth::set_cookie(STATE_COOKIE, "", "/auth", 0, state.config.secure_cookies())
}

fn github_client(
    state: &AppState,
) -> AppResult<
    BasicClient<
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointSet,
    >,
> {
    let config = &state.config;
    Ok(
        BasicClient::new(ClientId::new(config.github.client_id.clone()))
            .set_client_secret(ClientSecret::new(config.github.client_secret.clone()))
            .set_auth_uri(AuthUrl::from_url(config.github_auth_url.clone()))
            .set_token_uri(TokenUrl::from_url(config.github_token_url.clone()))
            .set_redirect_uri(redirect_uri(state, Provider::GitHub)?),
    )
}

async fn google_metadata(state: &AppState) -> AppResult<CoreProviderMetadata> {
    let mut cache = state.google.0.lock().await;
    if let Some((at, metadata)) = cache.as_ref() {
        if at.elapsed() < DISCOVERY_TTL {
            return Ok(metadata.clone());
        }
    }
    let issuer = IssuerUrl::new(state.config.google_issuer.clone()).map_err(AppError::internal)?;
    let metadata = CoreProviderMetadata::discover_async(issuer, &state.http)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "Google discovery failed");
            AppError(ErrorCode::OAuthFailed)
        })?;
    *cache = Some((Instant::now(), metadata.clone()));
    Ok(metadata)
}

async fn start_github(State(state): State<AppState>) -> AppResult<Response> {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf) = github_client(&state)?
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("read:user".to_owned()))
        .add_scope(Scope::new("user:email".to_owned()))
        .set_pkce_challenge(challenge)
        .url();
    let flow = FlowState {
        p: Provider::GitHub.as_str().to_owned(),
        s: csrf.secret().clone(),
        v: verifier.secret().clone(),
        n: None,
        e: now_unix() + STATE_TTL_SECONDS,
    };
    Ok(see_other(url.as_str(), vec![state_cookie(&state, &flow)?]))
}

async fn start_google(State(state): State<AppState>) -> Response {
    match start_google_inner(&state).await {
        Ok(response) => response,
        Err(err) => fail(&state, err),
    }
}

async fn start_google_inner(state: &AppState) -> AppResult<Response> {
    let metadata = google_metadata(state).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(state.config.google.client_id.clone()),
        Some(ClientSecret::new(state.config.google.client_secret.clone())),
    )
    .set_redirect_uri(redirect_uri(state, Provider::Google)?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf, nonce) = client
        .authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(openidconnect::Scope::new("email".to_owned()))
        .add_scope(openidconnect::Scope::new("profile".to_owned()))
        .set_pkce_challenge(challenge)
        .url();
    let flow = FlowState {
        p: Provider::Google.as_str().to_owned(),
        s: csrf.secret().clone(),
        v: verifier.secret().clone(),
        n: Some(nonce.secret().clone()),
        e: now_unix() + STATE_TTL_SECONDS,
    };
    Ok(see_other(url.as_str(), vec![state_cookie(state, &flow)?]))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// A failed sign-in lands the browser on the portal with the error code.
fn fail(state: &AppState, err: AppError) -> Response {
    see_other(
        &format!("/?error={}", err.code().as_str()),
        vec![clear_state_cookie(state)],
    )
}

async fn callback_github(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    let result = async {
        let (code, flow) = check_callback(&state, &headers, query, Provider::GitHub)?;
        let profile = github_profile(&state, code, flow).await?;
        finish(&state, session, profile).await
    }
    .await;
    result.unwrap_or_else(|err| fail(&state, err))
}

async fn callback_google(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    let result = async {
        let (code, flow) = check_callback(&state, &headers, query, Provider::Google)?;
        let profile = google_profile(&state, code, flow).await?;
        finish(&state, session, profile).await
    }
    .await;
    result.unwrap_or_else(|err| fail(&state, err))
}

/// The callback's code and the flow it belongs to: the state cookie must be
/// ours, unexpired, for this provider, and name the same `state`.
fn check_callback(
    state: &AppState,
    headers: &HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
    provider: Provider,
) -> AppResult<(String, FlowState)> {
    use subtle::ConstantTimeEq;
    let Ok(Query(query)) = query else {
        return Err(AppError(ErrorCode::OAuthState));
    };
    let flow = auth::cookie(headers, STATE_COOKIE)
        .and_then(|v| crypto::verify(&state.config.session_key, STATE_PURPOSE, v))
        .and_then(|json| serde_json::from_slice::<FlowState>(&json).ok())
        .filter(|f| f.p == provider.as_str() && f.e >= now_unix())
        .ok_or(AppError(ErrorCode::OAuthState))?;
    let same_state = query
        .state
        .as_deref()
        .is_some_and(|s| bool::from(s.as_bytes().ct_eq(flow.s.as_bytes())));
    if !same_state {
        return Err(AppError(ErrorCode::OAuthState));
    }
    if query.error.is_some() {
        return Err(AppError(ErrorCode::OAuthDenied));
    }
    let code = query.code.ok_or(AppError(ErrorCode::OAuthState))?;
    Ok((code, flow))
}

fn provider_failure(what: &str, err: impl std::fmt::Display) -> AppError {
    tracing::warn!(error = %err, "{what}");
    AppError(ErrorCode::OAuthFailed)
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
    email: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

async fn github_profile(state: &AppState, code: String, flow: FlowState) -> AppResult<Profile> {
    let token = github_client(state)?
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(flow.v))
        .request_async(&state.http)
        .await
        .map_err(|e| provider_failure("GitHub token exchange failed", e))?;
    let access = token.access_token().secret();
    let api = &state.config.github_api_url;
    let get = |path: &str| {
        state
            .http
            .get(format!("{api}{path}"))
            .bearer_auth(access)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
    };
    let user: GitHubUser = get("/user")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| provider_failure("GitHub /user failed", e))?
        .json()
        .await
        .map_err(|e| provider_failure("GitHub /user is not a user", e))?;
    let mut email = user.email.filter(|e| !e.is_empty());
    if email.is_none() {
        // A hidden primary email is still listed here (scope user:email);
        // it is display data, so a failure here is not a failed sign-in.
        let listed = get("/user/emails")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        if let Ok(response) = listed {
            if let Ok(emails) = response.json::<Vec<GitHubEmail>>().await {
                email = emails
                    .into_iter()
                    .find(|e| e.primary && e.verified)
                    .map(|e| e.email);
            }
        }
    }
    Ok(Profile {
        provider: Provider::GitHub,
        subject: user.id.to_string(),
        name: Some(user.login),
        email,
    })
}

async fn google_profile(state: &AppState, code: String, flow: FlowState) -> AppResult<Profile> {
    let metadata = google_metadata(state).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(state.config.google.client_id.clone()),
        Some(ClientSecret::new(state.config.google.client_secret.clone())),
    )
    .set_redirect_uri(redirect_uri(state, Provider::Google)?);
    let token = client
        .exchange_code(openidconnect::AuthorizationCode::new(code))
        .map_err(|e| provider_failure("Google has no token endpoint", e))?
        .set_pkce_verifier(openidconnect::PkceCodeVerifier::new(flow.v))
        .request_async(&state.http)
        .await
        .map_err(|e| provider_failure("Google token exchange failed", e))?;
    let id_token = token
        .extra_fields()
        .id_token()
        .ok_or_else(|| provider_failure("Google sent no id token", "missing id_token"))?;
    let nonce = Nonce::new(flow.n.unwrap_or_default());
    let claims = id_token
        .claims(&client.id_token_verifier(), &nonce)
        .map_err(|e| provider_failure("Google id token rejected", e))?;
    Ok(Profile {
        provider: Provider::Google,
        subject: claims.subject().as_str().to_owned(),
        name: claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.as_str().to_owned()),
        email: claims.email().map(|e| e.as_str().to_owned()),
    })
}

/// Who the profile signs in as, and whether a new session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A known identity: its user.
    SignedIn(Uuid),
    /// A new identity linked to the signed-in user.
    Linked(Uuid),
    /// A new identity and a new user.
    Created(Uuid),
}

impl Outcome {
    pub fn user_id(self) -> Uuid {
        match self {
            Outcome::SignedIn(u) | Outcome::Linked(u) | Outcome::Created(u) => u,
        }
    }
}

/// The account rule: a known (provider, subject) signs in as its user;
/// otherwise a signed-in user links it; otherwise it creates a user.
pub async fn resolve_account(
    db: &sqlx::PgPool,
    signed_in: Option<Uuid>,
    profile: &Profile,
) -> AppResult<Outcome> {
    let name = profile.name.as_deref().and_then(clean_display_name);
    // A race between two first sign-ins of one identity loses one insert to
    // the primary key; the second attempt then finds the identity.
    for _ in 0..2 {
        let mut tx = db.begin().await?;
        let known: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE identities SET name = $3, email = $4, updated_at = now() \
             WHERE provider = $1 AND subject = $2 RETURNING user_id",
        )
        .bind(profile.provider.as_str())
        .bind(&profile.subject)
        .bind(&name)
        .bind(&profile.email)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((user,)) = known {
            tx.commit().await?;
            return Ok(Outcome::SignedIn(user));
        }
        let (user, outcome) = match signed_in {
            Some(user) => (user, Outcome::Linked(user)),
            None => {
                let user = Uuid::now_v7();
                sqlx::query("INSERT INTO users (id, display_name) VALUES ($1, $2)")
                    .bind(user)
                    .bind(name.as_deref().unwrap_or(FALLBACK_NAME))
                    .execute(&mut *tx)
                    .await?;
                (user, Outcome::Created(user))
            }
        };
        let inserted = sqlx::query(
            "INSERT INTO identities (provider, subject, user_id, name, email) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(profile.provider.as_str())
        .bind(&profile.subject)
        .bind(user)
        .bind(&name)
        .bind(&profile.email)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {
                tx.commit().await?;
                return Ok(outcome);
            }
            Err(err) if is_unique_violation(&err, "identities_user_provider") => {
                return Err(AppError(ErrorCode::AlreadyLinked));
            }
            Err(err) if is_unique_violation(&err, "identities_pkey") => {
                tx.rollback().await?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(AppError::internal("identity insert raced twice"))
}

async fn finish(
    state: &AppState,
    session: Option<Session>,
    profile: Profile,
) -> AppResult<Response> {
    let signed_in = session.as_ref().map(|s| s.user_id);
    let outcome = resolve_account(&state.db, signed_in, &profile).await?;
    tracing::info!(
        provider = profile.provider.as_str(),
        user = %outcome.user_id(),
        ?outcome,
        "OAuth callback"
    );
    let mut cookies = vec![clear_state_cookie(state)];
    if signed_in != Some(outcome.user_id()) {
        // A new session for a new sign-in (never reuse one across users).
        if let Some(old) = &session {
            auth::end_session(&state.db, old, state.config.secure_cookies()).await?;
        }
        cookies.push(auth::start_session(state, outcome.user_id()).await?);
    }
    let location = match outcome {
        Outcome::Linked(_) => format!("/?linked={}", profile.provider.as_str()),
        _ => "/".to_owned(),
    };
    Ok(see_other(&location, cookies))
}

/// `POST /auth/logout`: ends the session. Its CSRF token comes as the
/// `X-CSRF-Token` header or a `csrf` form field. Always answers 303 to `/`.
async fn logout(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let secure = state.config.secure_cookies();
    let Some(session) = session else {
        return Ok(see_other("/", vec![auth::clear_session_cookie(secure)]));
    };
    let from_header = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
    let is_form = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    let from_form = if is_form {
        url::form_urlencoded::parse(&body)
            .find(|(k, _)| k == "csrf")
            .map(|(_, v)| v.into_owned())
    } else {
        None
    };
    session.check_csrf(from_header.or(from_form.as_deref()))?;
    let cleared = auth::end_session(&state.db, &session, secure).await?;
    Ok(see_other("/", vec![cleared]))
}
