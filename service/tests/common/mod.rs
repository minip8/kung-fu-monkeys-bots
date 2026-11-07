//! The test harness: the service in process on an ephemeral port over the
//! per-test database `#[sqlx::test]` creates, a fake GitHub and a fake Google
//! (OIDC discovery, JWKS and id tokens signed with a test RSA key) on another
//! port, and a cookie-keeping "browser" that never follows redirects.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::header::{AUTHORIZATION, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use kfm_ladder::config::{Config, OAuthApp, S3Config};
use kfm_ladder::AppState;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserEmail, EndUserName, IssuerUrl, JsonWebKeyId,
    LocalizedClaim, Nonce, PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::net::TcpListener;

pub const GITHUB_ID: &str = "gh-client";
pub const GITHUB_SECRET: &str = "gh-secret";
pub const GOOGLE_ID: &str = "google-client";
pub const GOOGLE_SECRET: &str = "google-secret";
const TEST_KEY: &str = include_str!("../fixtures/test_rsa_key.pem");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    Google,
}

impl Provider {
    pub fn slug(self) -> &'static str {
        match self {
            Provider::GitHub => "github",
            Provider::Google => "google",
        }
    }
}

/// The person the fake provider signs in next.
#[derive(Debug, Clone)]
pub struct FakeUser {
    pub subject: String,
    /// GitHub's login or Google's name.
    pub name: String,
    pub email: Option<String>,
    /// GitHub: hide the email from /user, list it in /user/emails.
    pub hide_email: bool,
}

impl FakeUser {
    pub fn new(subject: &str, name: &str) -> FakeUser {
        FakeUser {
            subject: subject.to_owned(),
            name: name.to_owned(),
            email: Some(format!(
                "{}@example.org",
                name.to_lowercase().replace(' ', ".")
            )),
            hide_email: false,
        }
    }
}

struct Grant {
    user: FakeUser,
    challenge: String,
    redirect_uri: String,
    nonce: Option<String>,
}

#[derive(Default)]
struct FakeInner {
    next: HashMap<&'static str, FakeUser>,
    codes: HashMap<String, Grant>,
    access: HashMap<String, FakeUser>,
    counter: u64,
    /// Google: sign the next id token with another nonce than the request's.
    wrong_nonce: bool,
    /// The authorize endpoint answers with ?error=access_denied.
    deny: bool,
    /// Requests the fake saw, by path, for assertions.
    seen: Vec<String>,
}

#[derive(Clone)]
pub struct Fake {
    pub base: String,
    inner: Arc<Mutex<FakeInner>>,
    key: Arc<CoreRsaPrivateSigningKey>,
}

impl Fake {
    pub fn set_next(&self, provider: Provider, user: FakeUser) {
        self.inner
            .lock()
            .unwrap()
            .next
            .insert(provider.slug(), user);
    }

    pub fn set_wrong_nonce(&self, on: bool) {
        self.inner.lock().unwrap().wrong_nonce = on;
    }

    pub fn set_deny(&self, on: bool) {
        self.inner.lock().unwrap().deny = on;
    }

    pub fn seen(&self) -> Vec<String> {
        self.inner.lock().unwrap().seen.clone()
    }

    pub fn google_issuer(&self) -> String {
        format!("{}/google", self.base)
    }
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize(fake: &Fake, provider: &'static str, q: &HashMap<String, String>) -> Response {
    let mut inner = fake.inner.lock().unwrap();
    inner.seen.push(format!("{provider}/authorize"));
    let client_id = if provider == "github" {
        GITHUB_ID
    } else {
        GOOGLE_ID
    };
    assert_eq!(q.get("client_id").map(String::as_str), Some(client_id));
    assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(
        q.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let scope = q.get("scope").cloned().unwrap_or_default();
    if provider == "google" {
        assert!(scope.split(' ').any(|s| s == "openid"), "scope {scope}");
        assert!(q.contains_key("nonce"));
    } else {
        assert!(scope.contains("read:user"), "scope {scope}");
    }
    let redirect_uri = q["redirect_uri"].clone();
    let state = q["state"].clone();
    let mut location = url::Url::parse(&redirect_uri).unwrap();
    if inner.deny {
        location
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &state);
        return (StatusCode::FOUND, [(LOCATION, location.to_string())]).into_response();
    }
    let user = inner
        .next
        .get(provider)
        .cloned()
        .expect("the test set the next user");
    inner.counter += 1;
    let code = format!("code-{provider}-{}", inner.counter);
    inner.codes.insert(
        code.clone(),
        Grant {
            user,
            challenge: q["code_challenge"].clone(),
            redirect_uri: redirect_uri.clone(),
            nonce: q.get("nonce").cloned(),
        },
    );
    location
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &state);
    (StatusCode::FOUND, [(LOCATION, location.to_string())]).into_response()
}

/// The client credentials, from HTTP Basic or the form.
fn client_credentials(headers: &HeaderMap, form: &HashMap<String, String>) -> (String, String) {
    if let Some(basic) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic)
            .unwrap();
        let text = String::from_utf8(decoded).unwrap();
        let (id, secret) = text.split_once(':').unwrap();
        let unescape = |s: &str| {
            url::form_urlencoded::parse(format!("x={s}").as_bytes())
                .next()
                .unwrap()
                .1
                .into_owned()
        };
        return (unescape(id), unescape(secret));
    }
    (
        form.get("client_id").cloned().unwrap_or_default(),
        form.get("client_secret").cloned().unwrap_or_default(),
    )
}

fn oauth_error(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({ "error": code }))).into_response()
}

fn token(
    fake: &Fake,
    provider: &'static str,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    let mut inner = fake.inner.lock().unwrap();
    inner.seen.push(format!("{provider}/token"));
    let (id, secret) = client_credentials(headers, form);
    let expected = if provider == "github" {
        (GITHUB_ID, GITHUB_SECRET)
    } else {
        (GOOGLE_ID, GOOGLE_SECRET)
    };
    if (id.as_str(), secret.as_str()) != expected {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    let Some(grant) = inner
        .codes
        .remove(form.get("code").map_or("", String::as_str))
    else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if pkce_challenge(&verifier) != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    inner.counter += 1;
    let access = format!("at-{provider}-{}", inner.counter);
    inner.access.insert(access.clone(), grant.user.clone());
    if provider == "github" {
        return Json(serde_json::json!({
            "access_token": access, "token_type": "bearer", "scope": "read:user,user:email"
        }))
        .into_response();
    }
    let nonce = if inner.wrong_nonce {
        "not-the-nonce".to_owned()
    } else {
        grant.nonce.clone().unwrap_or_default()
    };
    let now = chrono::Utc::now();
    let mut name = LocalizedClaim::new();
    name.insert(None, EndUserName::new(grant.user.name.clone()));
    let mut standard = StandardClaims::new(SubjectIdentifier::new(grant.user.subject.clone()))
        .set_name(Some(name));
    if let Some(email) = &grant.user.email {
        standard = standard.set_email(Some(EndUserEmail::new(email.clone())));
    }
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(fake.google_issuer()).unwrap(),
        vec![Audience::new(GOOGLE_ID.to_owned())],
        now + chrono::Duration::minutes(5),
        now,
        standard,
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(nonce)));
    let id_token = CoreIdToken::new(
        claims,
        fake.key.as_ref(),
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .unwrap();
    Json(serde_json::json!({
        "access_token": access, "token_type": "Bearer", "expires_in": 3600,
        "id_token": id_token.to_string(),
    }))
    .into_response()
}

fn bearer_user(fake: &Fake, headers: &HeaderMap) -> Option<FakeUser> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    fake.inner.lock().unwrap().access.get(token).cloned()
}

async fn github_user(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    fake.inner
        .lock()
        .unwrap()
        .seen
        .push("github/user".to_owned());
    assert!(
        headers.contains_key("user-agent"),
        "GitHub needs a User-Agent"
    );
    let Some(user) = bearer_user(&fake, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let email = if user.hide_email {
        None
    } else {
        user.email.clone()
    };
    Json(serde_json::json!({
        "id": user.subject.parse::<u64>().unwrap(),
        "login": user.name,
        "name": format!("{} Display", user.name),
        "email": email,
    }))
    .into_response()
}

async fn github_emails(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    fake.inner
        .lock()
        .unwrap()
        .seen
        .push("github/user/emails".to_owned());
    let Some(user) = bearer_user(&fake, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut list = vec![serde_json::json!({
        "email": "old@example.org", "primary": false, "verified": true
    })];
    if let Some(email) = user.email {
        list.push(serde_json::json!({ "email": email, "primary": true, "verified": true }));
    }
    Json(list).into_response()
}

async fn discovery(State(fake): State<Fake>) -> Json<serde_json::Value> {
    fake.inner
        .lock()
        .unwrap()
        .seen
        .push("google/discovery".to_owned());
    let issuer = fake.google_issuer();
    Json(serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    }))
}

async fn jwks(State(fake): State<Fake>) -> Json<CoreJsonWebKeySet> {
    Json(CoreJsonWebKeySet::new(vec![fake.key.as_verification_key()]))
}

async fn github_authorize(
    State(fake): State<Fake>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    authorize(&fake, "github", &q)
}

async fn google_authorize(
    State(fake): State<Fake>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    authorize(&fake, "google", &q)
}

async fn github_token(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    token(&fake, "github", &headers, &form)
}

async fn google_token(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    token(&fake, "google", &headers, &form)
}

/// Starts the fake GitHub (`/github/...`) and Google (`/google/...`).
pub async fn start_fake() -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let key = CoreRsaPrivateSigningKey::from_pem(TEST_KEY, Some(JsonWebKeyId::new("test".into())))
        .unwrap();
    let fake = Fake {
        base,
        inner: Arc::default(),
        key: Arc::new(key),
    };
    let router = Router::new()
        .route("/github/authorize", get(github_authorize))
        .route("/github/token", post(github_token))
        .route("/github/api/user", get(github_user))
        .route("/github/api/user/emails", get(github_emails))
        .route("/google/.well-known/openid-configuration", get(discovery))
        .route("/google/jwks", get(jwks))
        .route("/google/authorize", get(google_authorize))
        .route("/google/token", post(google_token))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    fake
}

pub struct TestApp {
    pub base: String,
    pub db: PgPool,
    pub fake: Fake,
}

pub fn config(public_url: &str, fake: &Fake) -> Config {
    Config {
        database_url: "postgres://unused".to_owned(),
        listen: "127.0.0.1:0".parse().unwrap(),
        public_url: public_url.to_owned(),
        session_key: [0x5a; 32],
        github: OAuthApp {
            client_id: GITHUB_ID.to_owned(),
            client_secret: GITHUB_SECRET.to_owned(),
        },
        google: OAuthApp {
            client_id: GOOGLE_ID.to_owned(),
            client_secret: GOOGLE_SECRET.to_owned(),
        },
        github_auth_url: format!("{}/github/authorize", fake.base).parse().unwrap(),
        github_token_url: format!("{}/github/token", fake.base).parse().unwrap(),
        github_api_url: format!("{}/github/api", fake.base),
        google_issuer: fake.google_issuer(),
        s3: S3Config {
            endpoint: "http://127.0.0.1:9000".parse().unwrap(),
            bucket: "kfm".to_owned(),
            access_key: "minio".to_owned(),
            secret_key: "minio-secret".to_owned(),
            region: "auto".to_owned(),
        },
        engine_tag: "v0.5.1".to_owned(),
        arena: None,
        sentry_dsn: None,
    }
}

/// The service over `db`, with `extra` routes merged in (tests' probes).
pub async fn start_with(db: PgPool, extra: Router<AppState>) -> TestApp {
    let fake = start_fake().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let state = AppState::new(db.clone(), config(&base, &fake));
    let router = kfm_ladder::routes().merge(extra).with_state(state);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    TestApp { base, db, fake }
}

pub async fn start(db: PgPool) -> TestApp {
    start_with(db, Router::new()).await
}

/// A browser: keeps the service's cookies, follows no redirect.
pub struct Browser {
    pub http: reqwest::Client,
    pub cookies: HashMap<String, String>,
    pub base: String,
}

impl Browser {
    pub fn new(app: &TestApp) -> Browser {
        Browser {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            cookies: HashMap::new(),
            base: app.base.clone(),
        }
    }

    fn url(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("http") {
            path_or_url.to_owned()
        } else {
            format!("{}{path_or_url}", self.base)
        }
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn absorb(&mut self, response: &reqwest::Response) {
        if !response.url().as_str().starts_with(&self.base) {
            return;
        }
        for value in response.headers().get_all(SET_COOKIE) {
            let text = value.to_str().unwrap();
            let (pair, attrs) = text.split_once(';').unwrap_or((text, ""));
            let (name, val) = pair.split_once('=').unwrap();
            if attrs.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), val.to_owned());
            }
        }
    }

    pub async fn send(&mut self, request: reqwest::RequestBuilder) -> reqwest::Response {
        let request = if self.cookies.is_empty() {
            request
        } else {
            request.header("cookie", self.cookie_header())
        };
        let response = request.send().await.unwrap();
        self.absorb(&response);
        response
    }

    pub async fn get(&mut self, path: &str) -> reqwest::Response {
        let request = self.http.get(self.url(path));
        self.send(request).await
    }

    pub async fn post_form(&mut self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        let request = self.http.post(self.url(path)).form(form);
        self.send(request).await
    }

    /// A JSON request, with the session's CSRF header when `csrf` is given.
    pub async fn json(
        &mut self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        csrf: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self.http.request(method, self.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        self.send(request).await
    }

    /// Runs a sign-in through the fake provider; returns where it landed.
    pub async fn oauth(&mut self, app: &TestApp, provider: Provider, user: FakeUser) -> String {
        app.fake.set_next(provider, user);
        let start = self.get(&format!("/auth/{}", provider.slug())).await;
        assert_eq!(start.status(), StatusCode::SEE_OTHER, "start");
        let authorize = location(&start);
        assert!(authorize.starts_with(&app.fake.base), "{authorize}");
        let at_provider = self.get(&authorize).await;
        assert_eq!(at_provider.status(), StatusCode::FOUND);
        let callback = location(&at_provider);
        let done = self.get(&callback).await;
        assert_eq!(done.status(), StatusCode::SEE_OTHER, "callback");
        location(&done)
    }

    /// Signs in and checks it landed on the portal without an error.
    pub async fn sign_in(&mut self, app: &TestApp, provider: Provider, user: FakeUser) {
        let landed = self.oauth(app, provider, user).await;
        assert!(!landed.contains("error"), "sign-in failed: {landed}");
    }

    /// `GET /me` as this browser.
    pub async fn me(&mut self) -> serde_json::Value {
        let response = self.get("/me").await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    pub async fn csrf(&mut self) -> String {
        self.me().await["csrf"].as_str().unwrap().to_owned()
    }

    pub async fn user_id(&mut self) -> String {
        self.me().await["id"].as_str().unwrap().to_owned()
    }
}

pub fn location(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(LOCATION)
        .expect("a redirect")
        .to_str()
        .unwrap()
        .to_owned()
}

/// An API error's code, checking the documented shape.
pub async fn error_code(response: reqwest::Response) -> String {
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["message"].is_string(), "{body}");
    assert_eq!(body.as_object().unwrap().len(), 2, "{body}");
    body["error"].as_str().unwrap().to_owned()
}

/// A browser signed in as a fresh GitHub user.
pub async fn github_browser(app: &TestApp, id: u64, login: &str) -> Browser {
    let mut browser = Browser::new(app);
    browser
        .sign_in(app, Provider::GitHub, FakeUser::new(&id.to_string(), login))
        .await;
    browser
}
