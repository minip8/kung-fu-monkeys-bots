//! Teams: the ladder's entrants. A team has owners and members; the creator
//! is its owner; an owner adds members; a member leaves (or an owner removes
//! them); the last owner cannot leave. An account is in at most one team
//! (v1.5), unless it is an operator's (`users.operator`): the house teams.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::{is_unique_violation, AppError, AppResult, ErrorCode};
use crate::extract::{ApiPath, JsonBody};
use crate::tokens::Scope;
use crate::users::find_user;
use crate::AppState;

pub const MIN_NAME_CHARS: usize = 3;
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_MEMBERS: i64 = 32;

/// A team in a list of the caller's teams, with the caller's role.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TeamSummary {
    pub id: Uuid,
    pub name: String,
    pub role: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Member {
    pub user_id: Uuid,
    pub display_name: String,
    pub role: String,
    #[serde(with = "time::serde::rfc3339")]
    pub joined_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub members: Vec<Member>,
    /// The active submission's id (`POST /teams/{t}/active`), if any.
    pub active: Option<Uuid>,
    /// Its standing in the current season, `null` before its first game
    /// there or outside a season (v1.2).
    pub rating: Option<crate::ladder::TeamRating>,
}

/// A team name: trimmed, 3 to 32 characters of ASCII letters, digits, space,
/// `-`, `_` and `.`, starting and ending with a letter or digit.
pub fn clean_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    let ok_len = (MIN_NAME_CHARS..=MAX_NAME_CHARS).contains(&name.len());
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.'));
    let ok_ends = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    if ok_len && ok_chars && ok_ends && !name.contains("  ") {
        Ok(name.to_owned())
    } else {
        Err(AppError(ErrorCode::InvalidName))
    }
}

/// The one-team rule (v1.5): locks the user's row, so two joins of one
/// account serialise, and refuses with `already_in_team` when the account is
/// in a team already and is not an operator's. `not_found` if the user does
/// not exist.
async fn ensure_may_join(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: Uuid,
) -> AppResult<()> {
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT operator FROM users WHERE id = $1 FOR UPDATE")
            .bind(user)
            .fetch_optional(&mut **tx)
            .await?;
    let (operator,) = row.ok_or(AppError(ErrorCode::NotFound))?;
    if operator {
        return Ok(());
    }
    let (in_team,): (bool,) =
        sqlx::query_as("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1)")
            .bind(user)
            .fetch_one(&mut **tx)
            .await?;
    if in_team {
        return Err(AppError(ErrorCode::AlreadyInTeam));
    }
    Ok(())
}

pub async fn create_team(db: &PgPool, owner: Uuid, name: &str) -> AppResult<Team> {
    let name = clean_name(name)?;
    let id = Uuid::now_v7();
    let mut tx = db.begin().await?;
    ensure_may_join(&mut tx, owner).await?;
    let created: Result<(OffsetDateTime,), sqlx::Error> = sqlx::query_as(
        "INSERT INTO teams (id, name, created_by) VALUES ($1, $2, $3) RETURNING created_at",
    )
    .bind(id)
    .bind(&name)
    .bind(owner)
    .fetch_one(&mut *tx)
    .await;
    let (created_at,) = match created {
        Ok(row) => row,
        Err(err) if is_unique_violation(&err, "teams_name_lower") => {
            return Err(AppError(ErrorCode::NameTaken))
        }
        Err(err) => return Err(err.into()),
    };
    sqlx::query("INSERT INTO memberships (team_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let members = members(db, id).await?;
    Ok(Team {
        id,
        name,
        created_at,
        members,
        active: None,
        rating: None,
    })
}

pub async fn my_teams(db: &PgPool, user: Uuid) -> AppResult<Vec<TeamSummary>> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.name, m.role, t.created_at FROM memberships m \
         JOIN teams t ON t.id = m.team_id WHERE m.user_id = $1 ORDER BY lower(t.name)",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

async fn members(db: &PgPool, team: Uuid) -> AppResult<Vec<Member>> {
    Ok(sqlx::query_as(
        "SELECT m.user_id, u.display_name, m.role, m.joined_at FROM memberships m \
         JOIN users u ON u.id = m.user_id WHERE m.team_id = $1 \
         ORDER BY m.role DESC, m.joined_at, m.user_id",
    )
    .bind(team)
    .fetch_all(db)
    .await?)
}

/// A team's name, or none when no team has the id (the games page's title).
pub async fn team_name(db: &PgPool, id: Uuid) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT name FROM teams WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?)
}

pub async fn get_team(db: &PgPool, id: Uuid) -> AppResult<Team> {
    let row: Option<(String, OffsetDateTime)> =
        sqlx::query_as("SELECT name, created_at FROM teams WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    let (name, created_at) = row.ok_or(AppError(ErrorCode::NotFound))?;
    Ok(Team {
        id,
        name,
        created_at,
        members: members(db, id).await?,
        active: crate::submissions::active_of(db, id).await?,
        rating: crate::ladder::team_rating(db, id).await?,
    })
}

/// Locks the team row (membership changes of one team serialise on it) and
/// returns the caller's role in it; `not_found` if the team does not exist.
async fn lock_team(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team: Uuid,
    user: Uuid,
) -> AppResult<Option<String>> {
    let exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
        .bind(team)
        .fetch_optional(&mut **tx)
        .await?;
    if exists.is_none() {
        return Err(AppError(ErrorCode::NotFound));
    }
    let role: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE team_id = $1 AND user_id = $2")
            .bind(team)
            .bind(user)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(role.map(|(r,)| r))
}

/// An owner adds the user named by `reference` (an id or a display name) as a member.
pub async fn add_member(db: &PgPool, caller: Uuid, team: Uuid, reference: &str) -> AppResult<Team> {
    let mut tx = db.begin().await?;
    if lock_team(&mut tx, team, caller).await?.as_deref() != Some("owner") {
        return Err(AppError(ErrorCode::Forbidden));
    }
    let user = find_user(&mut tx, reference).await?;
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM memberships WHERE team_id = $1")
        .bind(team)
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_MEMBERS {
        return Err(AppError(ErrorCode::LimitReached));
    }
    // A member of this team is `already_member`, whatever else it is in.
    let (member,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM memberships WHERE team_id = $1 AND user_id = $2)",
    )
    .bind(team)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    if member {
        return Err(AppError(ErrorCode::AlreadyMember));
    }
    ensure_may_join(&mut tx, user).await?;
    let added = sqlx::query(
        "INSERT INTO memberships (team_id, user_id, role) VALUES ($1, $2, 'member') \
         ON CONFLICT (team_id, user_id) DO NOTHING",
    )
    .bind(team)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    if added.rows_affected() == 0 {
        return Err(AppError(ErrorCode::AlreadyMember));
    }
    tx.commit().await?;
    get_team(db, team).await
}

/// `user` leaves `team` (`caller` == `user`) or an owner removes them.
pub async fn remove_member(db: &PgPool, caller: Uuid, team: Uuid, user: Uuid) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let caller_role = lock_team(&mut tx, team, caller).await?;
    if caller != user && caller_role.as_deref() != Some("owner") {
        return Err(AppError(ErrorCode::Forbidden));
    }
    let target: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE team_id = $1 AND user_id = $2")
            .bind(team)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((target_role,)) = target else {
        return Err(AppError(ErrorCode::NotFound));
    };
    if target_role == "owner" {
        let (owners,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM memberships WHERE team_id = $1 AND role = 'owner'",
        )
        .bind(team)
        .fetch_one(&mut *tx)
        .await?;
        if owners <= 1 {
            return Err(AppError(ErrorCode::LastOwner));
        }
    }
    sqlx::query("DELETE FROM memberships WHERE team_id = $1 AND user_id = $2")
        .bind(team)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateRequest {
    pub name: String,
}

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub user: String,
}

/// `POST /teams` (session + CSRF header).
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CreateRequest>,
) -> AppResult<(StatusCode, Json<Team>)> {
    caller.session_write(&headers)?;
    let team = create_team(&state.db, caller.user_id, &body.name).await?;
    Ok((StatusCode::CREATED, Json(team)))
}

/// `GET /teams`: the caller's teams (session or a token of either scope).
pub async fn list_mine(
    State(state): State<AppState>,
    caller: Caller,
) -> AppResult<Json<serde_json::Value>> {
    caller.require_scope(Scope::Read)?;
    let teams = my_teams(&state.db, caller.user_id).await?;
    Ok(Json(serde_json::json!({ "teams": teams })))
}

/// `GET /teams/{id}`: public.
pub async fn show(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> AppResult<Json<Team>> {
    Ok(Json(get_team(&state.db, id).await?))
}

/// `POST /teams/{id}/members` (session + CSRF header, an owner).
pub async fn add_member_route(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    ApiPath(id): ApiPath<Uuid>,
    JsonBody(body): JsonBody<AddMemberRequest>,
) -> AppResult<Json<Team>> {
    caller.session_write(&headers)?;
    Ok(Json(
        add_member(&state.db, caller.user_id, id, &body.user).await?,
    ))
}

/// `DELETE /teams/{id}/members/{user}` (session + CSRF header).
pub async fn remove_member_route(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    ApiPath((id, user)): ApiPath<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    caller.session_write(&headers)?;
    remove_member(&state.db, caller.user_id, id, user).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_names_follow_the_rules() {
        assert_eq!(clean_name(" Banana Squad ").unwrap(), "Banana Squad");
        for ok in ["abc", "a-b", "x_y.z", &"n".repeat(32)] {
            assert!(clean_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "ab",
            &"n".repeat(33),
            "-abc",
            "abc.",
            "a  b",
            "naïve",
            "a/b",
            "<script>",
        ] {
            assert!(clean_name(bad).is_err(), "{bad}");
        }
    }
}
