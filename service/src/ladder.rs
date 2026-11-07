//! The ladder's public reads (`docs/ladder-api.md` §10, v1.2): the board of
//! a season (`GET /ladder`), matches (`GET /matches`, `GET /matches/{m}`)
//! and a team's standing in the current season. Every route here is public:
//! ratings, results and replays are the contest's record. A browser asking
//! for HTML (`Accept: text/html`) gets the portal's page of the same route
//! instead ([`crate::portal`]).

use std::time::Duration;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::header::ACCEPT;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, AppResult, ErrorCode};
use crate::extract::ApiPath;
use crate::glicko::{self, Rating};
use crate::presign_limit::PresignQuota;
use crate::seasons::{self, Season, SeasonView};
use crate::AppState;

/// Teams per page of `GET /ladder`.
pub const LADDER_PAGE: i64 = 50;
/// Matches per page of `GET /matches`.
pub const MATCH_PAGE: i64 = 25;
/// How long a presigned replay URL stays valid.
pub const REPLAY_URL_TTL: Duration = Duration::from_secs(3600);

/// Whether the request prefers the portal's HTML (a browser's navigation
/// sends `Accept: text/html,...`; the CLI and curl do not).
pub fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept
                .split(',')
                .any(|part| part.trim().starts_with("text/html"))
        })
}

/// One row of the board.
#[derive(Debug, Clone, Serialize)]
pub struct Standing {
    pub rank: i64,
    pub team_id: Uuid,
    pub name: String,
    /// The rating on Glicko's scale, rounded.
    pub rating: i64,
    /// The deviation on Glicko's scale, rounded.
    pub rd: i64,
    pub games: i32,
    pub wins: i32,
    pub draws: i32,
    pub losses: i32,
    /// The team's active submission now (not necessarily the one that
    /// played every game) and its module's SHA-256.
    pub active: Option<Uuid>,
    pub module_hash: Option<String>,
}

#[derive(sqlx::FromRow)]
struct StandingRow {
    team_id: Uuid,
    name: String,
    mu: f64,
    phi: f64,
    games: i32,
    wins: i32,
    draws: i32,
    losses: i32,
    active: Option<Uuid>,
    module_hash: Option<Vec<u8>>,
}

fn rounded(x: f64) -> i64 {
    x.round() as i64
}

/// The board of `season`, page `page` (1-based): teams with at least one
/// game, the higher rating first, then the smaller deviation, then the team
/// id. Also whether a further page exists.
pub async fn standings(
    db: &PgPool,
    season: Uuid,
    page: i64,
    per_page: i64,
) -> AppResult<(Vec<Standing>, bool)> {
    let offset = (page - 1) * per_page;
    let rows: Vec<StandingRow> = sqlx::query_as(
        "SELECT r.team_id, t.name, r.mu, r.phi, r.games, r.wins, r.draws, r.losses, \
           a.submission_id AS active, s.module_hash \
         FROM ratings r JOIN teams t ON t.id = r.team_id \
         LEFT JOIN active_bots a ON a.team_id = r.team_id \
         LEFT JOIN submissions s ON s.id = a.submission_id \
         WHERE r.season_id = $1 AND r.games > 0 \
         ORDER BY r.mu DESC, r.phi ASC, r.team_id LIMIT $2 OFFSET $3",
    )
    .bind(season)
    .bind(per_page + 1)
    .bind(offset)
    .fetch_all(db)
    .await?;
    let more = rows.len() as i64 > per_page;
    let out = rows
        .into_iter()
        .take(per_page as usize)
        .enumerate()
        .map(|(i, r)| {
            let rating = Rating {
                mu: r.mu,
                phi: r.phi,
                sigma: glicko::DEFAULT_SIGMA,
            };
            Standing {
                rank: offset + i as i64 + 1,
                team_id: r.team_id,
                name: r.name,
                rating: rounded(rating.rating()),
                rd: rounded(rating.rd()),
                games: r.games,
                wins: r.wins,
                draws: r.draws,
                losses: r.losses,
                active: r.active,
                module_hash: r.module_hash.as_deref().map(hex::encode),
            }
        })
        .collect();
    Ok((out, more))
}

/// A team's standing in a season, as the team object's `rating` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct TeamRating {
    pub season: String,
    pub rank: i64,
    pub rating: i64,
    pub rd: i64,
    pub games: i32,
    pub wins: i32,
    pub draws: i32,
    pub losses: i32,
}

/// `team`'s standing in the current season, if it has played in it.
pub async fn team_rating(db: &PgPool, team: Uuid) -> AppResult<Option<TeamRating>> {
    let Some(season) = seasons::current(db).await? else {
        return Ok(None);
    };
    let row: Option<(f64, f64, i32, i32, i32, i32, i64)> = sqlx::query_as(
        "SELECT r.mu, r.phi, r.games, r.wins, r.draws, r.losses, \
           (SELECT count(*) FROM ratings o WHERE o.season_id = r.season_id AND o.games > 0 \
              AND (o.mu > r.mu OR (o.mu = r.mu AND (o.phi < r.phi OR (o.phi = r.phi AND o.team_id < r.team_id))))) \
         FROM ratings r WHERE r.season_id = $1 AND r.team_id = $2 AND r.games > 0",
    )
    .bind(season.id)
    .bind(team)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(mu, phi, games, wins, draws, losses, above)| {
        let rating = Rating {
            mu,
            phi,
            sigma: glicko::DEFAULT_SIGMA,
        };
        TeamRating {
            season: season.name.clone(),
            rank: above + 1,
            rating: rounded(rating.rating()),
            rd: rounded(rating.rd()),
            games,
            wins,
            draws,
            losses,
        }
    }))
}

/// One side of a recorded game.
#[derive(Debug, Clone, Serialize)]
pub struct MatchSide {
    pub team_id: Uuid,
    pub name: String,
    pub submission_id: Uuid,
    /// The mode's team index this side played (0 or 1).
    pub team: i16,
    pub score: i32,
    pub kills: i32,
    pub deaths: i32,
    /// The side's module slots that faulted: `{"slot","traps","over","bad","dead"}`.
    pub faults: Value,
    /// The side's rating change from this game, on Glicko's scale, to a
    /// tenth.
    pub rating_change: f64,
}

/// A recorded game as `GET /matches` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct MatchSummary {
    pub id: Uuid,
    pub season: Option<String>,
    pub season_id: Option<Uuid>,
    pub round: Option<i32>,
    /// kfm_arena's match index in its job: 0, then 1 with the sides swapped.
    pub game: i16,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub engine_tag: String,
    pub seed: i64,
    pub ticks: i32,
    /// `ended` or `cap` (the tick cap ended it; a draw).
    pub end: String,
    /// The winning team, or null for a draw.
    pub winner: Option<Uuid>,
    /// Side A (the higher ranked when scheduled) first.
    pub sides: [MatchSide; 2],
}

/// A recorded game as `GET /matches/{m}` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct MatchDetail {
    #[serde(flatten)]
    pub summary: MatchSummary,
    pub content_hash: String,
    pub build_hash: Option<String>,
    /// The match object kfm_arena printed (the engine's docs/arena-result.md).
    pub result: Value,
    /// A presigned URL of the `.kfmr`, valid for an hour from the response.
    pub replay_url: Option<String>,
    /// The replay in the client of the game's own release.
    pub viewer_url: Option<String>,
}

#[derive(sqlx::FromRow)]
struct MatchRow {
    id: Uuid,
    season_id: Option<Uuid>,
    season: Option<String>,
    round: Option<i32>,
    game: i16,
    created_at: OffsetDateTime,
    engine_tag: String,
    seed: i64,
    ticks: i32,
    ending: String,
    winner_team: Option<Uuid>,
    team_a: Uuid,
    name_a: String,
    submission_a: Uuid,
    side_a: i16,
    score_a: i32,
    kills_a: i32,
    deaths_a: i32,
    faults_a: Value,
    delta_a: f64,
    team_b: Uuid,
    name_b: String,
    submission_b: Uuid,
    score_b: i32,
    kills_b: i32,
    deaths_b: i32,
    faults_b: Value,
    delta_b: f64,
    content_hash: String,
    build_hash: Option<String>,
    replay_key: Option<String>,
}

const MATCH_SELECT: &str = "SELECT m.id, m.season_id, se.name AS season, m.round, m.game, m.created_at, \
     m.engine_tag, m.seed, m.ticks, m.ending, m.winner_team, \
     m.team_a, ta.name AS name_a, m.submission_a, m.side_a, m.score_a, m.kills_a, m.deaths_a, m.faults_a, m.delta_a, \
     m.team_b, tb.name AS name_b, m.submission_b, m.score_b, m.kills_b, m.deaths_b, m.faults_b, m.delta_b, \
     m.content_hash, m.build_hash, m.replay_key \
     FROM matches m JOIN teams ta ON ta.id = m.team_a JOIN teams tb ON tb.id = m.team_b \
     LEFT JOIN seasons se ON se.id = m.season_id";

fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

impl MatchRow {
    fn summary(&self) -> MatchSummary {
        MatchSummary {
            id: self.id,
            season: self.season.clone(),
            season_id: self.season_id,
            round: self.round,
            game: self.game,
            created_at: self.created_at,
            engine_tag: self.engine_tag.clone(),
            seed: self.seed,
            ticks: self.ticks,
            end: self.ending.clone(),
            winner: self.winner_team,
            sides: [
                MatchSide {
                    team_id: self.team_a,
                    name: self.name_a.clone(),
                    submission_id: self.submission_a,
                    team: self.side_a,
                    score: self.score_a,
                    kills: self.kills_a,
                    deaths: self.deaths_a,
                    faults: self.faults_a.clone(),
                    rating_change: tenth(self.delta_a),
                },
                MatchSide {
                    team_id: self.team_b,
                    name: self.name_b.clone(),
                    submission_id: self.submission_b,
                    team: 1 - self.side_a,
                    score: self.score_b,
                    kills: self.kills_b,
                    deaths: self.deaths_b,
                    faults: self.faults_b.clone(),
                    rating_change: tenth(self.delta_b),
                },
            ],
        }
    }
}

/// A page of matches, newest first, of `team` (either side) and `season`
/// when given; also whether a further page exists.
pub async fn list_matches(
    db: &PgPool,
    team: Option<Uuid>,
    season: Option<Uuid>,
    page: i64,
    per_page: i64,
) -> AppResult<(Vec<MatchSummary>, bool)> {
    let rows: Vec<MatchRow> = sqlx::query_as(&format!(
        "{MATCH_SELECT} WHERE ($1::uuid IS NULL OR m.team_a = $1 OR m.team_b = $1) \
         AND ($2::uuid IS NULL OR m.season_id = $2) \
         ORDER BY m.created_at DESC, m.id DESC LIMIT $3 OFFSET $4"
    ))
    .bind(team)
    .bind(season)
    .bind(per_page + 1)
    .bind((page - 1) * per_page)
    .fetch_all(db)
    .await?;
    let more = rows.len() as i64 > per_page;
    Ok((
        rows.iter()
            .take(per_page as usize)
            .map(MatchRow::summary)
            .collect(),
        more,
    ))
}

/// The viewer link of a replay: `<viewer>/<tag>/?replay=<url, encoded>`.
pub fn viewer_url(viewer: &str, tag: &str, replay_url: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(replay_url.as_bytes()).collect();
    format!("{}/{tag}/?replay={encoded}", viewer.trim_end_matches('/'))
}

/// One recorded game with its result and fresh replay links.
pub async fn match_detail(state: &AppState, id: Uuid) -> AppResult<MatchDetail> {
    let row: MatchRow = sqlx::query_as(&format!("{MATCH_SELECT} WHERE m.id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError(ErrorCode::NotFound))?;
    let (result,): (Value,) = sqlx::query_as("SELECT result FROM matches WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let replay_url = match &row.replay_key {
        Some(key) => Some(
            state
                .store
                .presign_get(key, REPLAY_URL_TTL)
                .await
                .map_err(AppError::internal)?,
        ),
        None => None,
    };
    let viewer_url = replay_url
        .as_deref()
        .map(|u| viewer_url(&state.config.viewer_url, &row.engine_tag, u));
    Ok(MatchDetail {
        summary: row.summary(),
        content_hash: row.content_hash.clone(),
        build_hash: row.build_hash.clone(),
        result,
        replay_url,
        viewer_url,
    })
}

/// The `season` query: a season by name, or the shown one (the current
/// season, else the last one that started); `None` before any season.
pub async fn season_param(db: &PgPool, name: Option<&str>) -> AppResult<Option<Season>> {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => Ok(Some(
            seasons::by_name(db, name)
                .await?
                .ok_or(AppError(ErrorCode::NotFound))?,
        )),
        None => Ok(seasons::shown(db).await?),
    }
}

/// `?page=`: 1-based, default 1.
pub fn page_param(page: Option<&str>) -> AppResult<i64> {
    match page {
        None | Some("") => Ok(1),
        Some(text) => match text.parse::<i64>() {
            Ok(n) if (1..=1_000_000).contains(&n) => Ok(n),
            _ => Err(AppError(ErrorCode::BadRequest)),
        },
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct LadderQuery {
    pub season: Option<String>,
    pub page: Option<String>,
}

/// The JSON of `GET /ladder`.
pub async fn ladder_json(state: &AppState, query: &LadderQuery) -> AppResult<Value> {
    let page = page_param(query.page.as_deref())?;
    let season = season_param(&state.db, query.season.as_deref()).await?;
    let now = seasons::db_now(&state.db).await?;
    let Some(season) = season else {
        return Ok(
            serde_json::json!({ "season": null, "page": page, "ratings": [], "next": null }),
        );
    };
    let (ratings, more) = standings(&state.db, season.id, page, LADDER_PAGE).await?;
    Ok(serde_json::json!({
        "season": SeasonView::of(&season, now),
        "page": page,
        "ratings": ratings,
        "next": if more { Some(page + 1) } else { None },
    }))
}

/// `GET /ladder?season=<name>&page=<n>` (auth: none).
pub async fn ladder_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<LadderQuery>, QueryRejection>,
) -> AppResult<Response> {
    let Query(query) = query.map_err(|_| AppError(ErrorCode::BadRequest))?;
    if wants_html(&headers) {
        return crate::portal::ladder_page(&state, &query).await;
    }
    Ok(Json(ladder_json(&state, &query).await?).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct MatchesQuery {
    pub team: Option<String>,
    pub season: Option<String>,
    pub page: Option<String>,
}

/// `GET /matches?team=<id>&season=<name>&page=<n>` (auth: none): newest
/// first; every season unless `season` is given.
pub async fn matches_route(
    State(state): State<AppState>,
    query: Result<Query<MatchesQuery>, QueryRejection>,
) -> AppResult<Json<Value>> {
    let Query(query) = query.map_err(|_| AppError(ErrorCode::BadRequest))?;
    let page = page_param(query.page.as_deref())?;
    let team = match query.team.as_deref().filter(|t| !t.is_empty()) {
        Some(text) => Some(Uuid::parse_str(text).map_err(|_| AppError(ErrorCode::BadRequest))?),
        None => None,
    };
    let season = match query.season.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(name) => Some(
            seasons::by_name(&state.db, name)
                .await?
                .ok_or(AppError(ErrorCode::NotFound))?
                .id,
        ),
        None => None,
    };
    let (matches, more) = list_matches(&state.db, team, season, page, MATCH_PAGE).await?;
    Ok(Json(serde_json::json!({
        "page": page,
        "matches": matches,
        "next": if more { Some(page + 1) } else { None },
    })))
}

/// `GET /matches/{m}` (auth: none). It presigns the replay URL, so it counts
/// against the caller's [`PresignQuota`] (the HTML page too).
pub async fn match_route(
    _quota: PresignQuota,
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiPath(id): ApiPath<Uuid>,
) -> AppResult<Response> {
    let detail = match_detail(&state, id).await?;
    if wants_html(&headers) {
        return crate::portal::match_page(&detail);
    }
    Ok(Json(detail).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_gets_html_and_a_program_json() {
        let mut headers = HeaderMap::new();
        assert!(!wants_html(&headers));
        headers.insert(ACCEPT, "*/*".parse().unwrap());
        assert!(!wants_html(&headers));
        headers.insert(ACCEPT, "application/json".parse().unwrap());
        assert!(!wants_html(&headers));
        headers.insert(
            ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
                .parse()
                .unwrap(),
        );
        assert!(wants_html(&headers));
    }

    #[test]
    fn the_viewer_link_names_the_release_and_encodes_the_replay() {
        let url = viewer_url(
            "https://kfm.minip8.com/",
            "v0.5.1",
            "https://r2.example/kfm/replays/x.kfmr?X-Amz-Signature=a&b=c",
        );
        assert_eq!(
            url,
            "https://kfm.minip8.com/v0.5.1/?replay=https%3A%2F%2Fr2.example%2Fkfm%2Freplays%2Fx.kfmr%3FX-Amz-Signature%3Da%26b%3Dc"
        );
    }

    #[test]
    fn pages_are_one_based() {
        assert_eq!(page_param(None).unwrap(), 1);
        assert_eq!(page_param(Some("3")).unwrap(), 3);
        for bad in ["0", "-1", "x", "1.5"] {
            assert!(page_param(Some(bad)).is_err(), "{bad}");
        }
    }
}
