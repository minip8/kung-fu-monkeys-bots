//! `GET /`: the signed-in home (a dashboard of the caller's team, the games
//! being played and the latest results) or, signed out, the landing page,
//! which shows the same live panels. Both are the state at page load: there
//! is no script, so the page says when it was rendered.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::response::Response;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    game_line, html_page, messages, nav, season_line, standing_view, team_views, viewer_teams,
    GameLine, MessageQuery, SeasonLine, Shell, StandingView, TeamView,
};
use crate::auth::{MaybeSession, Session};
use crate::error::AppResult;
use crate::glicko::{self, Rating};
use crate::{ladder, seasons, submissions, users, AppState};

/// The games of the caller's team the home lists.
const HOME_MATCHES: usize = 5;
/// The latest games across the ladder the home shows.
const HOME_RESULTS: i64 = 6;
/// The teams and the games the landing page shows.
const LANDING_TEAMS: i64 = 5;
const LANDING_GAMES: i64 = 5;
/// The match jobs listed under Now playing.
const LIVE_LINES: i64 = 6;
/// The games the account panel's recent rating change covers.
const RECENT_GAMES: usize = 10;
/// The most points the rating sparkline draws (a long season is sampled).
const SPARK_POINTS: usize = 160;
/// The sparkline's drawing box (stretched to the panel's width).
const SPARK_W: f64 = 200.0;
const SPARK_H: f64 = 56.0;
const SPARK_PAD: f64 = 4.0;

// ---------------------------------------------------------------------------
// Queries.

/// A team's rating in `season` (Glicko's scale, unrounded) and the rating
/// change of each of its games in the season, oldest first. `None` when the
/// team has no rating row there.
pub(crate) async fn rating_history(
    db: &PgPool,
    season: Uuid,
    team: Uuid,
) -> AppResult<Option<(f64, Vec<f64>)>> {
    let mu: Option<(f64,)> =
        sqlx::query_as("SELECT mu FROM ratings WHERE season_id = $1 AND team_id = $2")
            .bind(season)
            .bind(team)
            .fetch_optional(db)
            .await?;
    let Some((mu,)) = mu else {
        return Ok(None);
    };
    let deltas: Vec<f64> = sqlx::query_scalar(
        "SELECT CASE WHEN team_a = $2 THEN delta_a ELSE delta_b END FROM matches \
         WHERE season_id = $1 AND (team_a = $2 OR team_b = $2) ORDER BY created_at, id",
    )
    .bind(season)
    .bind(team)
    .fetch_all(db)
    .await?;
    let rating = Rating {
        mu,
        phi: 1.0,
        sigma: glicko::DEFAULT_SIGMA,
    };
    Ok(Some((rating.rating(), deltas)))
}

/// A match job waiting for or held by a worker, with its two teams.
#[derive(sqlx::FromRow)]
pub(crate) struct LiveMatch {
    pub running: bool,
    pub round: Option<i32>,
    pub team_a: Uuid,
    pub name_a: String,
    pub team_b: Uuid,
    pub name_b: String,
    /// When a worker claimed it (running), else when it was queued.
    pub since: OffsetDateTime,
    /// How many such jobs there are in all.
    pub total: i64,
}

/// The `match` jobs queued or running, the running ones first, then the
/// oldest; at most `limit`. A job whose payload does not name two existing
/// teams is left out.
pub(crate) async fn live_matches(db: &PgPool, limit: i64) -> AppResult<Vec<LiveMatch>> {
    Ok(sqlx::query_as(
        "SELECT j.state = 'running' AS running, \
           CASE WHEN j.payload->>'round' ~ '^[0-9]{1,9}$' THEN (j.payload->>'round')::int END AS round, \
           ta.id AS team_a, ta.name AS name_a, tb.id AS team_b, tb.name AS name_b, \
           coalesce(j.claimed_at, j.created_at) AS since, count(*) OVER () AS total \
         FROM jobs j \
         JOIN teams ta ON ta.id = CASE WHEN j.payload->>'team_a' ~* '^[0-9a-f-]{36}$' \
           THEN (j.payload->>'team_a')::uuid END \
         JOIN teams tb ON tb.id = CASE WHEN j.payload->>'team_b' ~* '^[0-9a-f-]{36}$' \
           THEN (j.payload->>'team_b')::uuid END \
         WHERE j.kind = 'match' AND j.state IN ('queued', 'running') \
         ORDER BY j.state = 'running' DESC, j.created_at, j.id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// When the last round of `season` was made, if it has one.
pub(crate) async fn last_round_at(db: &PgPool, season: Uuid) -> AppResult<Option<OffsetDateTime>> {
    Ok(
        sqlx::query_scalar("SELECT max(created_at) FROM rounds WHERE season_id = $1")
            .bind(season)
            .fetch_one(db)
            .await?,
    )
}

// ---------------------------------------------------------------------------
// The views.

/// A rating's history as an inline SVG sparkline.
struct Sparkline {
    /// The polyline's points and the filled area's path, in the drawing box.
    points: String,
    area: String,
    /// The season's starting rating as a dashed rule (its y), when the line
    /// crosses or touches it.
    base_y: Option<String>,
    games: usize,
    /// The rating before the season's first game and now, rounded.
    from: i64,
    to: i64,
    /// The change over the last [`RECENT_GAMES`] games (or fewer), signed.
    recent: String,
    recent_games: usize,
    /// `good`, `bad` or `muted`: the change's colour.
    recent_class: &'static str,
}

/// The ratings before each game and after the last: `now` walked back
/// through `deltas` (each game's change, oldest first).
fn rating_series(now: f64, deltas: &[f64]) -> Vec<f64> {
    let mut series = Vec::with_capacity(deltas.len() + 1);
    let mut r = now;
    series.push(r);
    for d in deltas.iter().rev() {
        r -= d;
        series.push(r);
    }
    series.reverse();
    series
}

/// At most `max` of `series`' points, evenly spaced, the first and the last
/// kept.
fn sampled(series: &[f64], max: usize) -> Vec<f64> {
    if series.len() <= max || max < 2 {
        return series.to_vec();
    }
    let last = series.len() - 1;
    (0..max)
        .map(|i| series[(i * last + (max - 1) / 2) / (max - 1)])
        .collect()
}

fn sparkline(now: f64, deltas: &[f64]) -> Option<Sparkline> {
    if deltas.is_empty() {
        return None;
    }
    let series = rating_series(now, deltas);
    let shown = sampled(&series, SPARK_POINTS);
    let lo = shown.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = shown.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = hi - lo;
    let y = |v: f64| {
        if span < 1e-9 {
            SPARK_H / 2.0
        } else {
            SPARK_PAD + (hi - v) / span * (SPARK_H - 2.0 * SPARK_PAD)
        }
    };
    let step = SPARK_W / (shown.len() - 1) as f64;
    let coords: Vec<String> = shown
        .iter()
        .enumerate()
        .map(|(i, &v)| format!("{:.1},{:.1}", i as f64 * step, y(v)))
        .collect();
    let points = coords.join(" ");
    let area = format!("M0,{SPARK_H} L{} L{SPARK_W},{SPARK_H} Z", coords.join(" L"));
    let start = series[0];
    let base_y = (span >= 1e-9 && start > lo && start < hi).then(|| format!("{:.1}", y(start)));
    let recent_games = deltas.len().min(RECENT_GAMES);
    let recent: f64 = deltas[deltas.len() - recent_games..].iter().sum();
    let recent_rounded = (recent * 10.0).round() / 10.0;
    Some(Sparkline {
        points,
        area,
        base_y,
        games: deltas.len(),
        from: start.round() as i64,
        to: now.round() as i64,
        recent: super::change(recent_rounded),
        recent_games,
        recent_class: if recent_rounded > 0.0 {
            "good"
        } else if recent_rounded < 0.0 {
            "bad"
        } else {
            "muted"
        },
    })
}

/// The team's bot as the account panel shows it.
struct BotView {
    /// The active submission's module hash (12 hex digits), when one is active.
    active: Option<String>,
    /// The newest upload when it is newer than the active one and not ready:
    /// its state (`queued`, `building`, `failed`, `rejected`).
    pending: Option<String>,
}

async fn bot_view(db: &PgPool, team: Uuid) -> AppResult<BotView> {
    let rows = submissions::list_rows(db, team).await?;
    let active = rows.iter().find(|r| r.active).map(|r| {
        r.module_hash
            .as_deref()
            .map(|h| hex::encode(h).chars().take(12).collect())
            .unwrap_or_default()
    });
    let pending = rows
        .first()
        .filter(|r| !r.active && r.state != "ready")
        .map(|r| r.state.clone());
    Ok(BotView { active, pending })
}

/// The signed-in home's view of the caller and their team.
struct HomeView {
    name: String,
    /// The first of the caller's teams (an operator may have several), with
    /// its standing and its latest games.
    team: Option<TeamView>,
    spark: Option<Sparkline>,
    bot: Option<BotView>,
    /// How many teams the caller is in.
    team_count: usize,
    can_create_team: bool,
}

/// A match job as Now playing lists it.
struct LiveLine {
    a: String,
    b: String,
    a_mine: bool,
    b_mine: bool,
    running: bool,
    round: Option<i32>,
    since: String,
}

/// What both states of `/` show of the ladder now: the games being played,
/// when the next round is due, the latest results.
struct LiveView {
    /// The time the page was rendered, `HH:MM UTC`.
    as_of: String,
    playing: Vec<LiveLine>,
    /// Match jobs beyond those listed.
    more: i64,
    /// Whether a season is running (nothing is scheduled outside one).
    season_running: bool,
    /// When the next round is due, as "in about 12 min"; empty when it
    /// cannot be known (no round yet) or is past the season's end.
    next_round: String,
    /// The scheduler's round interval, as "30 min".
    every: String,
    /// The latest games across the ladder, the viewer's sides marked.
    results: Vec<GameLine>,
}

/// The landing page's other parts: the shown season and the top of its board.
struct LandingView {
    season: Option<SeasonLine>,
    top: Vec<StandingView>,
    /// The scheduler's round interval in whole minutes (0 under a minute).
    round_minutes: u64,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    shell: Shell,
    error: Option<String>,
    notice: Option<String>,
    /// Signed in: the home.
    home: Option<HomeView>,
    /// Signed out: the landing page.
    landing: Option<LandingView>,
    live: LiveView,
    /// The shown season's name, or empty.
    season: String,
    engine_tag: String,
}

/// `HH:MM`.
fn clock(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}", at.hour(), at.minute())
}

/// A duration ahead in whole seconds, roughly: "12 min", "3 h", "6 days".
fn roughly(seconds: i64) -> String {
    let minutes = (seconds + 59) / 60;
    if minutes < 90 {
        format!("{} min", minutes.max(1))
    } else if minutes < 48 * 60 {
        format!("{} h", (minutes + 30) / 60)
    } else {
        format!("{} days", (minutes + 12 * 60) / (24 * 60))
    }
}

/// The phrase for when the next round is due, or empty.
fn next_round_phrase(
    last: Option<OffsetDateTime>,
    every_seconds: u64,
    season_end: OffsetDateTime,
    now: OffsetDateTime,
) -> String {
    let Some(last) = last else {
        return String::new();
    };
    let due = last + time::Duration::seconds(i64::try_from(every_seconds).unwrap_or(i64::MAX / 2));
    if due >= season_end {
        return String::new();
    }
    let ahead = (due - now).whole_seconds();
    if ahead <= 0 {
        "any moment now".to_owned()
    } else {
        format!("in about {}", roughly(ahead))
    }
}

async fn live_view(
    state: &AppState,
    mine: &[Uuid],
    results: i64,
    now: OffsetDateTime,
) -> AppResult<LiveView> {
    let db = &state.db;
    let jobs = live_matches(db, LIVE_LINES).await?;
    let more = jobs
        .first()
        .map_or(0, |j| j.total - jobs.len() as i64)
        .max(0);
    let current = seasons::current(db).await?;
    let next_round = match &current {
        Some(s) => next_round_phrase(
            last_round_at(db, s.id).await?,
            state.config.round_seconds,
            s.ends_at,
            now,
        ),
        None => String::new(),
    };
    let (games, _) = ladder::list_matches(db, None, None, 1, results).await?;
    Ok(LiveView {
        as_of: format!("{} UTC", clock(now)),
        playing: jobs
            .iter()
            .map(|j| LiveLine {
                a: j.name_a.clone(),
                b: j.name_b.clone(),
                a_mine: mine.contains(&j.team_a),
                b_mine: mine.contains(&j.team_b),
                running: j.running,
                round: j.round,
                since: clock(j.since),
            })
            .collect(),
        more,
        season_running: current.is_some(),
        next_round,
        every: roughly(i64::try_from(state.config.round_seconds).unwrap_or(i64::MAX / 2)),
        results: games.iter().map(|m| game_line(m).mark(mine)).collect(),
    })
}

async fn home_view(state: &AppState, session: &Session) -> AppResult<HomeView> {
    let db = &state.db;
    let user = users::get_user(db, session.user_id).await?;
    let mut views = team_views(db, user.id).await?;
    let team_count = views.len();
    let mut team = (!views.is_empty()).then(|| views.swap_remove(0));
    let (mut spark, mut bot) = (None, None);
    if let Some(t) = &mut team {
        t.matches.truncate(HOME_MATCHES);
        if let Some(season) = seasons::current(db).await? {
            if let Some((now, deltas)) = rating_history(db, season.id, t.id).await? {
                spark = sparkline(now, &deltas);
            }
        }
        bot = Some(bot_view(db, t.id).await?);
    }
    Ok(HomeView {
        name: user.display_name,
        can_create_team: user.operator || team_count == 0,
        team,
        spark,
        bot,
        team_count,
    })
}

async fn landing_view(
    state: &AppState,
    season: Option<&seasons::Season>,
    now: OffsetDateTime,
) -> AppResult<LandingView> {
    let top = match season {
        Some(s) => {
            ladder::standings(&state.db, s.id, 1, LANDING_TEAMS)
                .await?
                .0
        }
        None => Vec::new(),
    };
    Ok(LandingView {
        season: season.map(|s| season_line(s, now)),
        top: top.into_iter().map(|r| standing_view(r, &[])).collect(),
        round_minutes: state.config.round_seconds / 60,
    })
}

/// `GET /`: the landing page when signed out; the home when signed in. Shows
/// `?error=<code>` (sign-in failures land here) and `?linked=`.
pub(super) async fn index(
    State(state): State<AppState>,
    MaybeSession(session): MaybeSession,
    query: Result<Query<MessageQuery>, QueryRejection>,
) -> AppResult<Response> {
    let (error, notice) = messages(query);
    let now = seasons::db_now(&state.db).await?;
    let shown = ladder::season_param(&state.db, None).await?;
    let (home, landing, live) = match &session {
        Some(session) => {
            let home = home_view(&state, session).await?;
            let mine = viewer_teams(&state.db, Some(session)).await?;
            let live = live_view(&state, &mine, HOME_RESULTS, now).await?;
            (Some(home), None, live)
        }
        None => {
            let landing = landing_view(&state, shown.as_ref(), now).await?;
            let live = live_view(&state, &[], LANDING_GAMES, now).await?;
            (None, Some(landing), live)
        }
    };
    html_page(&IndexPage {
        shell: Shell::load(&state.db, session.as_ref(), nav::HOME).await?,
        error,
        notice,
        home,
        landing,
        live,
        season: shown.map(|s| s.name).unwrap_or_default(),
        engine_tag: state.config.engine_tag.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_series_walks_back_from_the_current_rating() {
        let series = rating_series(1520.0, &[10.0, -5.0, 15.0]);
        assert_eq!(series, vec![1500.0, 1510.0, 1505.0, 1520.0]);
    }

    #[test]
    fn a_long_series_is_sampled_with_both_ends() {
        let series: Vec<f64> = (0..1000).map(f64::from).collect();
        let shown = sampled(&series, 160);
        assert_eq!(shown.len(), 160);
        assert_eq!(shown[0], 0.0);
        assert_eq!(shown[159], 999.0);
        assert!(shown.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(sampled(&series[..5], 160).len(), 5);
    }

    #[test]
    fn the_sparkline_spans_its_box_and_sums_the_recent_games() {
        let deltas = [12.0, -4.0, 8.0];
        let spark = sparkline(1516.0, &deltas).unwrap();
        assert!(spark.points.starts_with("0.0,"));
        assert!(spark.points.contains("200.0,4.0"), "{}", spark.points);
        assert!(spark.area.starts_with("M0,56 L0.0,") && spark.area.ends_with("L200,56 Z"));
        assert_eq!((spark.from, spark.to, spark.games), (1500, 1516, 3));
        assert_eq!(spark.recent, "+16.0");
        assert_eq!(spark.recent_class, "good");
        assert!(sparkline(1500.0, &[]).is_none());
        // A flat line sits in the middle.
        let flat = sparkline(1500.0, &[0.0, 0.0]).unwrap();
        assert!(flat.points.contains("100.0,28.0"));
        assert_eq!(flat.recent_class, "muted");
    }

    #[test]
    fn durations_read_roughly() {
        assert_eq!(roughly(10), "1 min");
        assert_eq!(roughly(12 * 60), "12 min");
        assert_eq!(roughly(3 * 3600), "3 h");
        assert_eq!(roughly(7 * 86400), "7 days");
    }

    #[test]
    fn the_next_round_is_due_an_interval_after_the_last() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let end = now + time::Duration::days(3);
        let last = now - time::Duration::minutes(18);
        assert_eq!(
            next_round_phrase(Some(last), 1800, end, now),
            "in about 12 min"
        );
        assert_eq!(
            next_round_phrase(Some(now - time::Duration::hours(1)), 1800, end, now),
            "any moment now"
        );
        assert_eq!(next_round_phrase(None, 1800, end, now), "");
        assert_eq!(next_round_phrase(Some(last), 7 * 86400, end, now), "");
    }
}
