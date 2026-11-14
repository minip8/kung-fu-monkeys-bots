//! The portal: server-rendered HTML (askama) over the same service functions
//! as the JSON routes. Forms post urlencoded to `/portal/...`, carry the
//! session's CSRF token in a `csrf` field and answer 303 back to `/` (with
//! `?error=<code>` on failure), except token creation, which renders the page
//! with the secret once. No JavaScript. Signed out, `/` is the landing page;
//! `/docs/contest` is the contest guide.

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
use crate::ladder::{self, LadderQuery, MatchDetail, MatchSummary, MatchesQuery, Standing};
use crate::oauth::Provider;
use crate::tokens::{self, Scope};
use crate::{manifest, seasons, submissions, teams, users, AppState};

/// The games a team's section lists.
const RECENT_MATCHES: i64 = 10;
/// The teams and the games the landing page shows.
const LANDING_TEAMS: i64 = 5;
const LANDING_GAMES: i64 = 5;
/// The games under a season's board.
const LADDER_GAMES: i64 = 8;

/// The engine release the contest guide (`templates/contest.html`) and the
/// arena drawings (`templates/maps/`, `tools/contest_maps.py`) describe. A
/// test holds it to `engine.lock`'s `tag=`, so an engine bump stops here
/// until the guide has been read against the new release's content.
pub const CONTEST_GUIDE_ENGINE: &str = "v0.10.0";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/privacy", get(privacy))
        .route("/docs/contest", get(contest))
        .route("/favicon.svg", get(favicon_svg))
        .route("/favicon.ico", get(favicon_ico))
        .route("/portal/tokens", post(create_token))
        .route("/portal/tokens/{id}/revoke", post(revoke_token))
        .route("/portal/teams", post(create_team))
        .route("/portal/teams/{id}/rename", post(rename_team))
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
    ranked_games: i32,
    ranked_faults: i32,
}

/// One of a team's recent games, from the team's side.
struct MatchLine {
    id: Uuid,
    when: String,
    opponent: String,
    outcome: &'static str,
    /// The team's score, then the opponent's.
    score: String,
    change: String,
    faulted: bool,
}

struct TeamView {
    id: Uuid,
    name: String,
    role: String,
    is_owner: bool,
    members: Vec<MemberView>,
    submissions: Vec<SubmissionView>,
    /// The current season's standing as a line, or empty.
    rating: String,
    matches: Vec<MatchLine>,
}

struct UserView {
    id: Uuid,
    name: String,
    csrf: String,
    providers: Vec<ProviderView>,
    tokens: Vec<TokenView>,
    teams: Vec<TeamView>,
    /// Whether the "Create team" form is shown: an account without a team,
    /// or an operator's (v1.5).
    can_create_team: bool,
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
    /// What a signed-out visitor sees instead of the account.
    landing: Option<LandingView>,
    engine_tag: String,
}

/// The landing page's live parts: the season, the top of its board and its
/// latest games.
struct LandingView {
    season: Option<SeasonLine>,
    top: Vec<StandingView>,
    games: Vec<GameLine>,
    /// The scheduler's round interval in whole minutes (0 under a minute).
    round_minutes: u64,
}

/// A recorded game as a list shows it: side A, the score, side B.
struct GameLine {
    id: Uuid,
    when: String,
    arena: String,
    a: String,
    b: String,
    a_score: i32,
    b_score: i32,
    a_won: bool,
    b_won: bool,
    ticks: i32,
    /// The tick cap ended it (a draw).
    capped: bool,
}

fn game_line(m: &MatchSummary) -> GameLine {
    let [a, b] = &m.sides;
    GameLine {
        id: m.id,
        when: when(m.created_at),
        arena: m.arena.clone().unwrap_or_default(),
        a: a.name.clone(),
        b: b.name.clone(),
        a_score: a.score,
        b_score: b.score,
        a_won: m.winner == Some(a.team_id),
        b_won: m.winner == Some(b.team_id),
        ticks: m.ticks,
        capped: m.end == "cap",
    }
}

/// A rating change for display: `+12.3`, `-4.0`.
fn change(delta: f64) -> String {
    format!("{delta:+.1}")
}

fn match_line(m: &MatchSummary, team: Uuid) -> MatchLine {
    let (mine, theirs) = if m.sides[0].team_id == team {
        (&m.sides[0], &m.sides[1])
    } else {
        (&m.sides[1], &m.sides[0])
    };
    MatchLine {
        id: m.id,
        when: when(m.created_at),
        opponent: theirs.name.clone(),
        outcome: match m.winner {
            None => "draw",
            Some(w) if w == team => "won",
            Some(_) => "lost",
        },
        score: format!("{}–{}", mine.score, theirs.score),
        change: change(mine.rating_change),
        faulted: mine.faults.as_array().is_some_and(|f| !f.is_empty()),
    }
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
                ranked_games: row.ranked_games,
                ranked_faults: row.ranked_faults,
                state: row.state,
            })
            .collect();
        let rating = team
            .rating
            .as_ref()
            .map(|r| {
                format!(
                    "Season {}: rank {}, rating {} ± {}, {} game(s): {} won, {} drawn, {} lost.",
                    r.season, r.rank, r.rating, r.rd, r.games, r.wins, r.draws, r.losses
                )
            })
            .unwrap_or_default();
        let (recent, _) = ladder::list_matches(db, Some(team.id), None, 1, RECENT_MATCHES).await?;
        let matches = recent.iter().map(|m| match_line(m, team.id)).collect();
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
            rating,
            matches,
        });
    }
    Ok(UserView {
        id: user.id,
        name: user.display_name,
        csrf: session.csrf.clone(),
        providers,
        tokens,
        can_create_team: user.operator || team_views.is_empty(),
        teams: team_views,
    })
}

/// A portal page: HTML, never cached (it may show a token's secret), framed by
/// nobody, and no scripts.
fn page_response(status: StatusCode, html: String) -> Response {
    page_response_csp(status, html, PAGE_CSP)
}

/// The portal pages' Content-Security-Policy: no script, forms to the portal
/// only, images from the service only (the favicon; nothing else is loaded).
const PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; \
     frame-ancestors 'none'; base-uri 'none'";

/// [`page_response`] with another Content-Security-Policy (the CLI login's
/// page, whose form redirects to the CLI's local listener).
pub(crate) fn page_response_csp(status: StatusCode, html: String, csp: &'static str) -> Response {
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (CONTENT_TYPE.as_str(), "text/html; charset=utf-8"),
        (CACHE_CONTROL.as_str(), "no-store"),
        ("content-security-policy", csp),
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
    let (user, landing) = match session {
        Some(session) => (Some(user_view(state, session).await?), None),
        None => (None, Some(landing_view(state).await?)),
    };
    let html = IndexPage {
        user,
        error,
        notice,
        new_token,
        landing,
        engine_tag: state.config.engine_tag.clone(),
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

async fn landing_view(state: &AppState) -> AppResult<LandingView> {
    let db = &state.db;
    let season = ladder::season_param(db, None).await?;
    let now = seasons::db_now(db).await?;
    let (top, games) = match &season {
        Some(s) => (
            ladder::standings(db, s.id, 1, LANDING_TEAMS).await?.0,
            ladder::list_matches(db, None, Some(s.id), 1, LANDING_GAMES)
                .await?
                .0,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(LandingView {
        season: season.as_ref().map(|s| season_line(s, now)),
        top: top.into_iter().map(standing_view).collect(),
        games: games.iter().map(game_line).collect(),
        round_minutes: state.config.round_seconds / 60,
    })
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

/// `POST /portal/teams/{id}/rename`: form `csrf`, `name` (v1.9).
async fn rename_team(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    ApiPath(id): ApiPath<Uuid>,
    FormBody(form): FormBody<TeamForm>,
) -> Response {
    done(
        async {
            let session = form_session(session, &form.csrf)?;
            teams::rename_team(&state.db, session.user_id, id, &form.name)
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

struct SeasonLine {
    name: String,
    mode: String,
    window: String,
    current: bool,
}

struct SeasonLink {
    name: String,
    /// The name, URL-encoded for `?season=`.
    query: String,
    selected: bool,
}

fn season_line(s: &seasons::Season, now: OffsetDateTime) -> SeasonLine {
    SeasonLine {
        name: s.name.clone(),
        mode: s.mode.clone(),
        window: format!("{} to {}", when(s.starts_at), when(s.ends_at)),
        current: s.holds(now),
    }
}

struct StandingView {
    rank: i64,
    team_id: Uuid,
    name: String,
    rating: i64,
    rd: i64,
    games: i32,
    wins: i32,
    draws: i32,
    losses: i32,
    hash: String,
}

fn standing_view(r: Standing) -> StandingView {
    StandingView {
        rank: r.rank,
        team_id: r.team_id,
        name: r.name,
        rating: r.rating,
        rd: r.rd,
        games: r.games,
        wins: r.wins,
        draws: r.draws,
        losses: r.losses,
        hash: r
            .module_hash
            .map(|h| h.chars().take(12).collect())
            .unwrap_or_default(),
    }
}

#[derive(Template)]
#[template(path = "ladder.html")]
struct LadderPage {
    title: String,
    season: Option<SeasonLine>,
    seasons: Vec<SeasonLink>,
    rows: Vec<StandingView>,
    /// The season's latest games, under the board.
    games: Vec<GameLine>,
    /// The season's page of `/matches`.
    games_url: String,
    prev: Option<String>,
    next: Option<String>,
    engine_tag: String,
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// `GET /ladder` for a browser: the board of a season, paged, with links to
/// the other seasons and the season's latest games.
pub async fn ladder_page(state: &AppState, query: &LadderQuery) -> AppResult<Response> {
    let page = ladder::page_param(query.page.as_deref())?;
    let season = ladder::season_param(&state.db, query.season.as_deref()).await?;
    let now = seasons::db_now(&state.db).await?;
    let all = seasons::list(&state.db).await?;
    let (rows, more) = match &season {
        Some(s) => ladder::standings(&state.db, s.id, page, ladder::LADDER_PAGE).await?,
        None => (Vec::new(), false),
    };
    let games = match &season {
        Some(s) => {
            ladder::list_matches(&state.db, None, Some(s.id), 1, LADDER_GAMES)
                .await?
                .0
        }
        None => Vec::new(),
    };
    let link = |p: i64| {
        let mut url = format!("/ladder?page={p}");
        if let Some(s) = &season {
            url.push_str("&season=");
            url.push_str(&encode(&s.name));
        }
        url
    };
    let html = LadderPage {
        title: season
            .as_ref()
            .map_or_else(|| "no season".to_owned(), |s| s.name.clone()),
        seasons: all
            .iter()
            .map(|s| SeasonLink {
                name: s.name.clone(),
                query: encode(&s.name),
                selected: season.as_ref().is_some_and(|c| c.id == s.id),
            })
            .collect(),
        season: season.as_ref().map(|s| season_line(s, now)),
        rows: rows.into_iter().map(standing_view).collect(),
        games: games.iter().map(game_line).collect(),
        games_url: season.as_ref().map_or_else(
            || "/matches".to_owned(),
            |s| format!("/matches?season={}", encode(&s.name)),
        ),
        prev: (page > 1).then(|| link(page - 1)),
        next: more.then(|| link(page + 1)),
        engine_tag: state.config.engine_tag.clone(),
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

struct SideView {
    label: &'static str,
    /// The team's name with the mode's team index it played.
    name: String,
    team_name: String,
    team: i16,
    won: bool,
    score: i32,
    kills: i32,
    deaths: i32,
    change: String,
    faults: Vec<String>,
}

#[derive(Template)]
#[template(path = "match.html")]
struct MatchPage {
    title: String,
    season: String,
    round: String,
    game: i16,
    arena: String,
    when: String,
    engine_tag: String,
    outcome: String,
    ending: &'static str,
    sides: Vec<SideView>,
    score_a: i32,
    score_b: i32,
    replay_url: Option<String>,
    viewer_url: Option<String>,
    seed: i64,
    ticks: i32,
    content_hash: String,
    build_hash: String,
    result: String,
}

fn fault_line(fault: &serde_json::Value) -> String {
    let n = |k: &str| {
        fault
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let mut parts = Vec::new();
    for key in ["traps", "over", "bad"] {
        if n(key) > 0 {
            parts.push(format!("{key} {}", n(key)));
        }
    }
    if let Some(dead) = fault.get("dead").and_then(serde_json::Value::as_str) {
        parts.push(format!("dead ({dead})"));
    }
    format!("slot {}: {}", n("slot"), parts.join(", "))
}

/// `GET /matches/{m}` for a browser: the game's sides, its replay links and
/// its raw result.
pub fn match_page(detail: &MatchDetail) -> AppResult<Response> {
    let m = &detail.summary;
    let outcome = match m.winner {
        None => "A draw".to_owned(),
        Some(w) => {
            let name = m
                .sides
                .iter()
                .find(|s| s.team_id == w)
                .map_or("?", |s| s.name.as_str());
            format!("{name} won")
        }
    };
    let html = MatchPage {
        title: format!("{} v {}", m.sides[0].name, m.sides[1].name),
        season: m.season.clone().unwrap_or_default(),
        round: m.round.map(|r| r.to_string()).unwrap_or_default(),
        game: m.game + 1,
        arena: m.arena.clone().unwrap_or_default(),
        when: when(m.created_at),
        engine_tag: m.engine_tag.clone(),
        outcome,
        ending: if m.end == "cap" {
            "The tick cap ended the game, so it counts as a draw"
        } else {
            "The game ended"
        },
        sides: m
            .sides
            .iter()
            .zip(["A", "B"])
            .map(|(s, label)| SideView {
                label,
                name: format!("{} (team {})", s.name, s.team),
                team_name: s.name.clone(),
                team: s.team,
                won: m.winner == Some(s.team_id),
                score: s.score,
                kills: s.kills,
                deaths: s.deaths,
                change: change(s.rating_change),
                faults: s
                    .faults
                    .as_array()
                    .map(|f| f.iter().map(fault_line).collect())
                    .unwrap_or_default(),
            })
            .collect(),
        score_a: m.sides[0].score,
        score_b: m.sides[1].score,
        replay_url: detail.replay_url.clone(),
        viewer_url: detail.viewer_url.clone(),
        seed: m.seed,
        ticks: m.ticks,
        content_hash: detail.content_hash.clone(),
        build_hash: detail.build_hash.clone().unwrap_or_default(),
        result: serde_json::to_string_pretty(&detail.result).map_err(AppError::internal)?,
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

#[derive(Template)]
#[template(path = "matches.html")]
struct MatchesPage {
    title: String,
    /// The team whose games these are, when `?team=` names one.
    team: Option<String>,
    season: Option<String>,
    /// Whether a team or a season narrows the list.
    filtered: bool,
    games: Vec<GameLine>,
    prev: Option<String>,
    next: Option<String>,
}

/// `GET /matches` for a browser: recorded games, newest first, paged, of
/// every team and season unless the query narrows it.
pub async fn matches_page(state: &AppState, query: &MatchesQuery) -> AppResult<Response> {
    let filter = ladder::matches_filter(&state.db, query).await?;
    let (matches, more) = ladder::list_matches(
        &state.db,
        filter.team,
        filter.season.as_ref().map(|s| s.id),
        filter.page,
        ladder::MATCH_PAGE,
    )
    .await?;
    let team = match filter.team {
        Some(id) => teams::team_name(&state.db, id).await?,
        None => None,
    };
    let link = |p: i64| {
        let mut url = format!("/matches?page={p}");
        if let Some(id) = filter.team {
            url.push_str(&format!("&team={id}"));
        }
        if let Some(s) = &filter.season {
            url.push_str("&season=");
            url.push_str(&encode(&s.name));
        }
        url
    };
    let html = MatchesPage {
        title: team
            .as_ref()
            .map_or_else(|| "Games".to_owned(), |name| format!("Games of {name}")),
        filtered: filter.team.is_some() || filter.season.is_some(),
        team,
        season: filter.season.as_ref().map(|s| s.name.clone()),
        games: matches.iter().map(game_line).collect(),
        prev: (filter.page > 1).then(|| link(filter.page - 1)),
        next: more.then(|| link(filter.page + 1)),
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

#[derive(Template)]
#[template(path = "contest.html")]
struct ContestPage {
    /// The release the guide was written for.
    guide_tag: &'static str,
    /// The release the service plays.
    engine_tag: String,
    /// Whether the two differ: the page says so.
    stale: bool,
    round_minutes: u64,
    round_opponents: usize,
    replay_hours: u64,
}

/// `GET /docs/contest` (auth: none): the contest guide.
async fn contest(State(state): State<AppState>) -> AppResult<Response> {
    let config = &state.config;
    let html = ContestPage {
        guide_tag: CONTEST_GUIDE_ENGINE,
        stale: config.engine_tag != CONTEST_GUIDE_ENGINE,
        engine_tag: config.engine_tag.clone(),
        round_minutes: config.round_seconds / 60,
        round_opponents: config.round_opponents,
        replay_hours: config.replay_hours,
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

/// The site's icon, generated by `tools/favicon.py` (never edited): the
/// header's mark, as an SVG and as an ICO for the clients that ask for
/// `/favicon.ico` whatever the page says.
const FAVICON_SVG: &[u8] = include_bytes!("../assets/favicon.svg");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");

/// A static image: cacheable for a day (a release may change it).
fn image_response(content_type: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            (CONTENT_TYPE.as_str(), content_type),
            (CACHE_CONTROL.as_str(), "public, max-age=86400"),
            ("x-content-type-options", "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /favicon.svg` (auth: none).
async fn favicon_svg() -> Response {
    image_response("image/svg+xml", FAVICON_SVG)
}

/// `GET /favicon.ico` (auth: none).
async fn favicon_ico() -> Response {
    image_response("image/x-icon", FAVICON_ICO)
}

/// The date the privacy page's text last changed.
pub const PRIVACY_DATE: &str = "2026-11-07";

#[derive(Template)]
#[template(path = "privacy.html")]
struct PrivacyPage {
    contact: String,
    /// A link for the contact when it is a URL or an email address.
    contact_href: Option<String>,
    replay_hours: u64,
    session_idle_days: i32,
    date: &'static str,
}

/// Where the contact links to: itself for an http(s) URL, `mailto:` for an
/// address, nothing for other text.
fn contact_href(contact: &str) -> Option<String> {
    if contact.starts_with("https://") || contact.starts_with("http://") {
        Some(contact.to_owned())
    } else if contact.contains('@') && !contact.contains(char::is_whitespace) {
        Some(format!("mailto:{contact}"))
    } else {
        None
    }
}

/// `GET /privacy` (auth: none): what the service stores and why.
async fn privacy(State(state): State<AppState>) -> AppResult<Response> {
    let contact = state.config.contact.clone();
    let html = PrivacyPage {
        contact_href: contact_href(&contact),
        contact,
        replay_hours: state.config.replay_hours,
        session_idle_days: crate::auth::SESSION_IDLE_DAYS,
        date: PRIVACY_DATE,
    }
    .render()
    .map_err(AppError::internal)?;
    Ok(page_response(StatusCode::OK, html))
}

#[cfg(test)]
mod tests {
    use super::CONTEST_GUIDE_ENGINE;

    /// The guide names the engine release it was written for; the pin moving
    /// on is the moment to read it (and redraw the maps) against the new one.
    #[test]
    fn the_contest_guide_is_for_the_pinned_engine() {
        let lock = include_str!("../../engine.lock");
        let tag = lock
            .lines()
            .find_map(|line| line.strip_prefix("tag="))
            .expect("engine.lock has a tag= line");
        assert_eq!(
            tag, CONTEST_GUIDE_ENGINE,
            "engine.lock moved: read service/templates/contest.html against the new release's \
             content/modes.toml and docs/bot-abi.md, run tools/contest_maps.py, then set \
             CONTEST_GUIDE_ENGINE"
        );
    }
}
