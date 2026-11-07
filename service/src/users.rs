//! Users and `GET /me`.

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::{Auth, Caller};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::AppState;

pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub display_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A linked provider account as `GET /me` shows it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Identity {
    pub provider: String,
    pub name: Option<String>,
    pub email: Option<String>,
    #[serde(rename = "linked_at", with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A display name from what a provider reports: control characters dropped,
/// whitespace collapsed, at most 64 characters; `None` if nothing is left.
pub fn clean_display_name(raw: &str) -> Option<String> {
    let collapsed = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_NAME_CHARS)
        .collect::<String>();
    let trimmed = collapsed.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub async fn get_user(db: &PgPool, id: Uuid) -> AppResult<User> {
    sqlx::query_as("SELECT id, display_name, created_at FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError(ErrorCode::NotFound))
}

pub async fn identities(db: &PgPool, user_id: Uuid) -> AppResult<Vec<Identity>> {
    Ok(sqlx::query_as(
        "SELECT provider, name, email, created_at FROM identities \
         WHERE user_id = $1 ORDER BY provider",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// A user named by id or by display name (ignoring case): `unknown_user` if
/// none, `ambiguous_user` if the name is shared.
pub async fn find_user(conn: &mut sqlx::PgConnection, reference: &str) -> AppResult<Uuid> {
    let reference = reference.trim();
    if let Ok(id) = Uuid::parse_str(reference) {
        let found: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
        if let Some((id,)) = found {
            return Ok(id);
        }
    }
    let found: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE lower(display_name) = lower($1) LIMIT 2")
            .bind(reference)
            .fetch_all(&mut *conn)
            .await?;
    match found.as_slice() {
        [(id,)] => Ok(*id),
        [] => Err(AppError(ErrorCode::UnknownUser)),
        _ => Err(AppError(ErrorCode::AmbiguousUser)),
    }
}

/// `GET /me` (session or a token of either scope).
pub async fn me(
    State(state): State<AppState>,
    caller: Caller,
) -> AppResult<Json<serde_json::Value>> {
    let user = get_user(&state.db, caller.user_id).await?;
    let identities = identities(&state.db, caller.user_id).await?;
    let mut body = serde_json::to_value(&user).map_err(AppError::internal)?;
    body["identities"] = serde_json::to_value(identities).map_err(AppError::internal)?;
    match &caller.auth {
        Auth::Session(session) => {
            body["auth"] = "session".into();
            body["csrf"] = session.csrf.clone().into();
        }
        Auth::Token { scope, .. } => {
            body["auth"] = "token".into();
            body["scope"] = scope.as_str().into();
        }
    }
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_cleaned() {
        assert_eq!(
            clean_display_name("  Ada \t Lovelace "),
            Some("Ada Lovelace".to_owned())
        );
        assert_eq!(clean_display_name("\u{7}\n "), None);
        assert_eq!(clean_display_name(&"x".repeat(100)).unwrap().len(), 64);
    }
}
