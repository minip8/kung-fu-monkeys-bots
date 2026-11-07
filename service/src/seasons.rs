//! Seasons (`docs/ladder-api.md` §10; architecture v0.59: a season is a time
//! window, not a release). A season is the window `[starts_at, ends_at)` and
//! the content mode its matches play (`contest`); windows never overlap (the
//! `seasons_no_overlap` exclusion), so at most one season is current at any
//! instant. Seasons are made by an operator on the VM (`kfm-ladder season
//! create`); there is no HTTP route that writes them.

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppResult;
use crate::AppState;

/// The mode a season plays when `--mode` is not given.
pub const DEFAULT_MODE: &str = "contest";
pub const MAX_NAME_CHARS: usize = 64;

/// A season row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Season {
    pub id: Uuid,
    pub name: String,
    pub mode: String,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
}

impl Season {
    /// Whether `at` falls inside the window.
    pub fn holds(&self, at: OffsetDateTime) -> bool {
        self.starts_at <= at && at < self.ends_at
    }
}

/// A season as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct SeasonView {
    pub id: Uuid,
    pub name: String,
    pub mode: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    pub current: bool,
}

impl SeasonView {
    pub fn of(season: &Season, now: OffsetDateTime) -> SeasonView {
        SeasonView {
            id: season.id,
            name: season.name.clone(),
            mode: season.mode.clone(),
            starts_at: season.starts_at,
            ends_at: season.ends_at,
            current: season.holds(now),
        }
    }
}

/// Why a season was not created.
#[derive(Debug)]
pub enum SeasonError {
    InvalidName,
    InvalidMode,
    BadTime(String),
    EmptyWindow,
    NameTaken,
    /// The window overlaps these seasons'.
    Overlaps(Vec<String>),
    Database(sqlx::Error),
}

impl std::fmt::Display for SeasonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeasonError::InvalidName => write!(
                f,
                "a season name is 1 to {MAX_NAME_CHARS} characters of ASCII letters, digits, space, '-', '_' and '.', starting and ending with a letter or digit"
            ),
            SeasonError::InvalidMode => {
                f.write_str("a mode is 1 to 32 characters of a-z, 0-9 and '_' (a content mode's name)")
            }
            SeasonError::BadTime(text) => {
                write!(f, "{text:?} is not an RFC 3339 time (2026-12-01T00:00:00Z)")
            }
            SeasonError::EmptyWindow => f.write_str("the end must be after the start"),
            SeasonError::NameTaken => f.write_str("a season of that name exists"),
            SeasonError::Overlaps(names) => write!(
                f,
                "the window overlaps the season(s) {}: seasons never overlap",
                names.join(", ")
            ),
            SeasonError::Database(err) => write!(f, "database: {err}"),
        }
    }
}

impl std::error::Error for SeasonError {}

impl From<sqlx::Error> for SeasonError {
    fn from(err: sqlx::Error) -> Self {
        SeasonError::Database(err)
    }
}

/// A season name: trimmed, then 1 to 64 characters of ASCII letters, digits,
/// space, `-`, `_` and `.`, starting and ending with a letter or digit.
pub fn clean_name(name: &str) -> Result<String, SeasonError> {
    let name = name.trim();
    let ok = (1..=MAX_NAME_CHARS).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.'))
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    if ok {
        Ok(name.to_owned())
    } else {
        Err(SeasonError::InvalidName)
    }
}

/// A content mode's name, as the `seasons_mode` CHECK has it.
pub fn valid_mode(mode: &str) -> bool {
    (1..=32).contains(&mode.len())
        && mode
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// An RFC 3339 time.
pub fn parse_time(text: &str) -> Result<OffsetDateTime, SeasonError> {
    OffsetDateTime::parse(text.trim(), &Rfc3339).map_err(|_| SeasonError::BadTime(text.to_owned()))
}

const COLUMNS: &str = "id, name, mode, starts_at, ends_at";

/// Creates a season. Refused when the name is taken or the window overlaps
/// another season's (checked first for the message, then by the exclusion
/// constraint, which also settles a race).
pub async fn create(
    db: &PgPool,
    name: &str,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
    mode: &str,
) -> Result<Season, SeasonError> {
    let name = clean_name(name)?;
    if !valid_mode(mode) {
        return Err(SeasonError::InvalidMode);
    }
    if ends_at <= starts_at {
        return Err(SeasonError::EmptyWindow);
    }
    let overlapping: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM seasons WHERE tstzrange(starts_at, ends_at, '[)') && tstzrange($1, $2, '[)') \
         ORDER BY starts_at",
    )
    .bind(starts_at)
    .bind(ends_at)
    .fetch_all(db)
    .await?;
    if !overlapping.is_empty() {
        return Err(SeasonError::Overlaps(
            overlapping.into_iter().map(|(n,)| n).collect(),
        ));
    }
    let inserted = sqlx::query_as(&format!(
        "INSERT INTO seasons (id, name, starts_at, ends_at, mode) VALUES ($1, $2, $3, $4, $5) \
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(&name)
    .bind(starts_at)
    .bind(ends_at)
    .bind(mode)
    .fetch_one(db)
    .await;
    match inserted {
        Ok(season) => Ok(season),
        Err(err) if crate::error::is_unique_violation(&err, "seasons_name") => {
            Err(SeasonError::NameTaken)
        }
        Err(sqlx::Error::Database(e))
            if e.code().as_deref() == Some("23P01")
                && e.constraint() == Some("seasons_no_overlap") =>
        {
            Err(SeasonError::Overlaps(
                vec!["(created meanwhile)".to_owned()],
            ))
        }
        Err(err) => Err(err.into()),
    }
}

/// Every season, the latest start first.
pub async fn list(db: &PgPool) -> Result<Vec<Season>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM seasons ORDER BY starts_at DESC, id DESC"
    ))
    .fetch_all(db)
    .await
}

/// The season whose window holds the database's `now()`, if any.
pub async fn current(db: &PgPool) -> Result<Option<Season>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM seasons WHERE starts_at <= now() AND now() < ends_at"
    ))
    .fetch_optional(db)
    .await
}

/// The season the ladder shows by default: the current one, else the one
/// that started last (frozen at its end), else none (no season has begun).
pub async fn shown(db: &PgPool) -> Result<Option<Season>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM seasons WHERE starts_at <= now() \
         ORDER BY (now() < ends_at) DESC, starts_at DESC LIMIT 1"
    ))
    .fetch_optional(db)
    .await
}

/// A season by name.
pub async fn by_name(db: &PgPool, name: &str) -> Result<Option<Season>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM seasons WHERE name = $1"))
        .bind(name.trim())
        .fetch_optional(db)
        .await
}

/// A season by id.
pub async fn by_id(db: &PgPool, id: Uuid) -> Result<Option<Season>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM seasons WHERE id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
}

/// The database's clock (the season windows are judged on it).
pub async fn db_now(db: &PgPool) -> Result<OffsetDateTime, sqlx::Error> {
    let (now,): (OffsetDateTime,) = sqlx::query_as("SELECT now()").fetch_one(db).await?;
    Ok(now)
}

/// `GET /seasons` (auth: none): every season, the latest start first.
pub async fn list_route(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let now = db_now(&state.db).await?;
    let seasons: Vec<SeasonView> = list(&state.db)
        .await?
        .iter()
        .map(|s| SeasonView::of(s, now))
        .collect();
    // v1.3: the release new games run under, so `kfm test` plays the same one.
    Ok(Json(serde_json::json!({
        "seasons": seasons,
        "engine_tag": state.config.engine_tag,
    })))
}

/// One line per season for `kfm-ladder season list`.
pub fn format_line(season: &Season, now: OffsetDateTime) -> String {
    let at = |t: OffsetDateTime| t.format(&Rfc3339).unwrap_or_else(|_| t.to_string());
    format!(
        "{}\t{}\t{}\t{}\t{}{}",
        season.name,
        at(season.starts_at),
        at(season.ends_at),
        season.mode,
        season.id,
        if season.holds(now) { "\tcurrent" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_modes_and_times_follow_the_rules() {
        assert_eq!(clean_name(" Season 1 ").unwrap(), "Season 1");
        for bad in ["", " ", "-s", "s.", "naïve", &"n".repeat(65)] {
            assert!(clean_name(bad).is_err(), "{bad}");
        }
        assert!(valid_mode("contest") && valid_mode("grid_rounds_1t"));
        for bad in ["", "Contest", "a-b", &"m".repeat(33)] {
            assert!(!valid_mode(bad), "{bad}");
        }
        assert!(parse_time("2026-12-01T00:00:00Z").is_ok());
        assert!(parse_time("2026-12-01T09:00:00+09:00").is_ok());
        assert!(parse_time("2026-12-01").is_err());
    }

    #[test]
    fn a_window_holds_its_start_not_its_end() {
        let start = parse_time("2026-12-01T00:00:00Z").unwrap();
        let end = parse_time("2027-01-01T00:00:00Z").unwrap();
        let season = Season {
            id: Uuid::nil(),
            name: "s".to_owned(),
            mode: DEFAULT_MODE.to_owned(),
            starts_at: start,
            ends_at: end,
        };
        assert!(season.holds(start));
        assert!(!season.holds(end));
        assert!(!season.holds(start - time::Duration::seconds(1)));
    }
}
