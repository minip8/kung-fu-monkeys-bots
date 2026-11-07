//! The portal: server-rendered HTML (askama) over the same service functions
//! as the JSON routes. Forms post urlencoded to `/portal/...`, carry the
//! session's CSRF token in a `csrf` field and answer 303 back to `/` (with
//! `?error=<code>` on failure), except token creation, which renders the page
//! with the secret once. No JavaScript.

use askama::Template;
use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::{MaybeSession, Session};
use crate::error::{ApiError, ApiResult, AppError, AppResult, ErrorCode};
use crate::extract::{ApiPath, FormBody};
use crate::oauth::Provider;
use crate::tokens::{self, Scope};
use crate::{manifest, submissions, teams, users, AppState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/portal/tokens", post(create_token))
        .route("/portal/tokens/{id}/revoke", post(revoke_token))
        .route("/portal/teams", post(create_team))
        .route("/portal/teams/{id}/members", post(add_member))
        .route(
            "/portal/teams/{id}/members/{user}/remove",
            post(remove_member),
        )
        .route(
            "/portal/teams/{id}/submissions",
            post(submit).layer(DefaultBodyLimit::max(submissions::MAX_UPLOAD_BYTES)),
        )
        .route("/portal/teams/{id}/active", post(activate))
}

struct ProviderView {
    slug: &'static str,
    label: &'static str,
    linked: bool,
    detail: String,
}

struct TokenView {
    id: Uuid,
    name: String,
    scope: String,
    created: String,
    last_used: String,
}

struct MemberView {
    user_id: Uuid,
    name: String,
    role: String,
    is_me: bool,
}

struct SubmissionView {
    id: Uuid,
    created: String,
    state: String,
    entry: String,
    /// The first 12 hex digits of the module's SHA-256, or empty.
    hash: String,
    error: String,
    has_log: bool,
    is_ready: bool,
    is_active: bool,
}

struct TeamView {
    id: Uuid,
    name: String,
    role: String,
    is_owner: bool,
    members: Vec<MemberView>,
    submissions: Vec<SubmissionView>,
}

struct UserView {
    id: Uuid,
    name: String,
    csrf: String,
    providers: Vec<ProviderView>,
    tokens: Vec<TokenView>,
    teams: Vec<TeamView>,
}

struct NewTokenView {
    name: String,
    scope: String,
    secret: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    user: Option<UserView>,
    error: Option<String>,
    notice: Option<String>,
    new_token: Option<NewTokenView>,
    engine_tag: String,
}

fn when(at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

async fn user_view(state: &AppState, session: &Session) -> AppResult<UserView> {
    let db = &state.db;
    let user = users::get_user(db, session.user_id).await?;
    let identities = users::identities(db, session.user_id).await?;
    let providers = Provider::ALL
        .iter()
        .map(|p| {
            let found = identities.iter().find(|i| i.provider == p.as_str());
            ProviderView {
                slug: p.as_str(),
                label: p.label(),
                linked: found.is_some(),
                detail: found
                    .map(|i| {
                        [i.name.as_deref(), i.email.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    let tokens = tokens::list_tokens(db, session.user_id)
        .await?
        .into_iter()
        .map(|t| TokenView {
            id: t.id,
            name: t.name,
            scope: t.scope,
            created: when(t.created_at),
            last_used: t.last_used_at.map_or_else(|| "never".to_owned(), when),
        })
        .collect();
    let mut team_views = Vec::new();
    for summary in teams::my_teams(db, session.user_id).await? {
        let team = teams::get_team(db, summary.id).await?;
        let submissions = submissions::list_rows(db, team.id)
            .await?
            .into_iter()
            .map(|row| SubmissionView {
                id: row.id,
                created: when(row.created_at),
                entry: manifest::parse(&row.manifest)
                    .map(|m| m.entry_display().to_owned())
                    .unwrap_or_default(),
                hash: row
                    .module_hash
                    .as_deref()
                    .map(|h| hex::encode(h).chars().take(12).collect())
                    .unwrap_or_default(),
                error: row.error.unwrap_or_default(),
                has_log: row.log_key.is_some(),
                is_ready: row.state == "ready",
                is_active: row.active,
                state: row.state,
            })
            .collect();
        team_views.push(TeamView {
            id: team.id,
            name: team.name,
            is_owner: summary.role == "owner",
            role: summary.role,
            members: team
                .members
                .into_iter()
                .map(|m| MemberView {
                    is_me: m.user_id == session.user_id,
                    user_id: m.user_id,
                    name: m.display_name,
                    role: m.role,
                })
                .collect(),
            submissions,
        });
    }
    Ok(UserView {
        id: user.id,
        name: user.display_name,
        csrf: session.csrf.clone(),
        providers,
        tokens,
        teams: team_views,
    })
}

/// A portal page: HTML, never cached (it may show a token's secret), framed by
/// nobody, and no scripts.
fn page(status: StatusCode, html: String) -> Response {
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (CONTENT_TYPE.as_str(), "text/html; charset=utf-8"),
        (CACHE_CONTROL.as_str(), "no-store"),
        (
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "same-origin"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn render(
    state: &AppState,
    session: Option<&Session>,
    error: Option<String>,
    notice: Option<String>,
    new_token: Option<NewTokenView>,
) -> AppResult<Response> {
    let user = match session {
        Some(session) => Some(user_view(state, session).await?),
        None => None,
    };
    let html = IndexPage {
        user,
        error,
        notice,
        new_token,
        engine_tag: state.config.engine_tag.clone(),
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page(StatusCode::OK, html))
}

#[derive(Deserialize)]
struct IndexQuery {
    error: Option<String>,
    /// What exactly was refused (`bad_manifest`, `bad_source` only).
    detail: Option<String>,
    linked: Option<String>,
}

/// The codes whose redirect may carry a detail.
const DETAILED: [ErrorCode; 2] = [ErrorCode::BadManifest, ErrorCode::BadSource];
/// The longest detail shown.
const MAX_DETAIL_CHARS: usize = 300;

/// `GET /`: sign in when signed out; the account, tokens and teams when signed in.
async fn index(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    query: Result<Query<IndexQuery>, QueryRejection>,
) -> AppResult<Response> {
    let query = query.ok().map(|Query(q)| q);
    let error = query
        .as_ref()
        .and_then(|q| q.error.as_deref())
        .and_then(ErrorCode::parse)
        .map(|code| {
            let detail = query
                .as_ref()
                .and_then(|q| q.detail.as_deref())
                .filter(|_| DETAILED.contains(&code))
                .map(|d| d.chars().take(MAX_DETAIL_CHARS).collect::<String>());
            ApiError { code, detail }.message()
        });
    let notice = query
        .as_ref()
        .and_then(|q| q.linked.as_deref())
        .and_then(|slug| Provider::ALL.into_iter().find(|p| p.as_str() == slug))
        .map(|p| format!("{} is now linked to your account.", p.label()));
    render(&state, session.as_ref(), error, notice, None).await
}

fn back(error: Option<AppError>) -> Response {
    back_detailed(error.map(ApiError::from))
}

/// Back to `/`, with `?error=<code>` and, for a detailed error, `&detail=`.
fn back_detailed(error: Option<ApiError>) -> Response {
    let location = match error {
        Some(err) => {
            let mut location = format!("/?error={}", err.code.as_str());
            if let Some(detail) = err.detail.filter(|_| DETAILED.contains(&err.code)) {
                let short: String = detail.chars().take(MAX_DETAIL_CHARS).collect();
                location.push_str("&detail=");
                location.extend(url::form_urlencoded::byte_serialize(short.as_bytes()));
            }
            location
        }
        None => "/".to_owned(),
    };
    (StatusCode::SEE_OTHER, [(LOCATION, location)]).into_response()
}

/// The session behind a form, its CSRF field checked.
fn form_session(session: Option<Session>, csrf: &str) -> AppResult<Session> {
    let session = session.ok_or(AppError(ErrorCode::Unauthenticated))?;
    session.check_csrf(Some(csrf))?;
    Ok(session)
}

/// Runs a form action and redirects back to `/`.
fn done(result: AppResult<()>) -> Response {
    back(result.err())
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf: String,
}

#[derive(Deserialize)]
struct TokenForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    scope: String,
}

async fn create_token(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    FormBody(form): FormBody<TokenForm>,
) -> Response {
    let result = async {
        let session = form_session(session, &form.csrf)?;
        let scope = Scope::parse(&form.scope).ok_or(AppError(ErrorCode::InvalidScope))?;
        let (info, secret) =
            tokens::create_token(&state.db, session.user_id, &form.name, scope).await?;
        let shown = NewTokenView {
            name: info.name,
            scope: info.scope,
            secret,
        };
        render(&state, Some(&session), None, None, Some(shown)).await
    }
    .await;
    result.unwrap_or_else(|err| back(Some(err)))
}

async fn revoke_token(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath(id): ApiPath<Uuid>,
    FormBody(form): FormBody<CsrfForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            tokens::revoke_token(&state.db, session.user_id, id).await
        }
        .await,
    )
}

#[derive(Deserialize)]
struct TeamForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    name: String,
}

async fn create_team(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    FormBody(form): FormBody<TeamForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            teams::create_team(&state.db, session.user_id, &form.name)
                .await
                .map(|_| ())
        }
        .await,
    )
}

#[derive(Deserialize)]
struct MemberForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    user: String,
}

async fn add_member(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath(id): ApiPath<Uuid>,
    FormBody(form): FormBody<MemberForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            teams::add_member(&state.db, session.user_id, id, &form.user)
                .await
                .map(|_| ())
        }
        .await,
    )
}

async fn remove_member(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath((id, user)): ApiPath<(Uuid, Uuid)>,
    FormBody(form): FormBody<CsrfForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            teams::remove_member(&state.db, session.user_id, id, user).await
        }
        .await,
    )
}

/// `POST /portal/teams/{id}/submissions`: multipart `csrf`, `language`,
/// `entry` and the file `source.zip`; the manifest is made from the fields.
async fn submit(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath(id): ApiPath<Uuid>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let result: ApiResult<()> = async {
        let session = session.ok_or(AppError(ErrorCode::Unauthenticated))?;
        let multipart = multipart.map_err(|_| AppError(ErrorCode::BadRequest))?;
        let upload = submissions::read_upload(multipart).await?;
        let field = |name: &str| {
            upload
                .fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        session.check_csrf(field("csrf"))?;
        let manifest = manifest::from_fields(
            field("language").unwrap_or("cpp"),
            field("entry").unwrap_or("."),
        );
        let zip = upload.zip.filter(|z| !z.is_empty()).ok_or_else(|| {
            ApiError::with_detail(ErrorCode::BadSource, "choose a source.zip to upload")
        })?;
        submissions::create_submission(&state, id, session.user_id, &manifest, &zip)
            .await
            .map(|_| ())
    }
    .await;
    back_detailed(result.err())
}

#[derive(Deserialize)]
struct ActivateForm {
    #[serde(default)]
    csrf: String,
    submission: Uuid,
}

async fn activate(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath(id): ApiPath<Uuid>,
    FormBody(form): FormBody<ActivateForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            submissions::activate(&state, id, session.user_id, form.submission)
                .await
                .map(|_| ())
        }
        .await,
    )
}
