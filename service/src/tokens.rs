//! API tokens: 256 random bits shown once as `kfm_<43 base64url>`, stored as
//! the SHA-256 of that string, scoped `submit` or `read`, sent as a bearer.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::Caller;
use crate::crypto;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::extract::{ApiPath, JsonBody};
use crate::AppState;

pub const TOKEN_PREFIX: &str = "kfm_";
/// Live (unrevoked) tokens per user.
pub const MAX_LIVE_TOKENS: i64 = 25;
pub const MAX_NAME_CHARS: usize = 64;

/// What a token may do. `submit` covers `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Submit,
    Read,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Submit => "submit",
            Scope::Read => "read",
        }
    }

    pub fn parse(text: &str) -> Option<Scope> {
        match text {
            "submit" => Some(Scope::Submit),
            "read" => Some(Scope::Read),
            _ => None,
        }
    }

    /// Whether a token of this scope may do what `needed` allows.
    pub fn covers(self, needed: Scope) -> bool {
        self == Scope::Submit || needed == Scope::Read
    }
}

/// A token as listed: never its secret.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TokenInfo {
    pub id: Uuid,
    pub name: String,
    pub scope: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

/// A name for a token: trimmed, 1 to 64 characters, no control characters.
pub fn clean_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(AppError(ErrorCode::InvalidName));
    }
    Ok(name.to_owned())
}

fn new_secret() -> String {
    format!("{TOKEN_PREFIX}{}", crypto::b64(&crypto::random::<32>()))
}

/// Whether `secret` has a token's shape (so a malformed bearer costs no query).
fn well_formed(secret: &str) -> bool {
    secret
        .strip_prefix(TOKEN_PREFIX)
        .and_then(crypto::unb64)
        .is_some_and(|bytes| bytes.len() == 32)
}

/// Creates a token; returns it and its secret (the only time the secret exists).
pub async fn create_token(
    db: &PgPool,
    user_id: Uuid,
    name: &str,
    scope: Scope,
) -> AppResult<(TokenInfo, String)> {
    let name = clean_name(name)?;
    let mut tx = db.begin().await?;
    // Serialise a user's token creations so the cap holds under concurrency.
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let (live,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM tokens WHERE user_id = $1 AND revoked_at IS NULL")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    if live >= MAX_LIVE_TOKENS {
        return Err(AppError(ErrorCode::LimitReached));
    }
    let secret = new_secret();
    let info: TokenInfo = sqlx::query_as(
        "INSERT INTO tokens (id, user_id, name, hash, scope) VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, name, scope, created_at, last_used_at",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(&name)
    .bind(&crypto::sha256(secret.as_bytes())[..])
    .bind(scope.as_str())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((info, secret))
}

/// The user's live tokens, oldest first.
pub async fn list_tokens(db: &PgPool, user_id: Uuid) -> AppResult<Vec<TokenInfo>> {
    Ok(sqlx::query_as(
        "SELECT id, name, scope, created_at, last_used_at FROM tokens \
         WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at, id",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// Revokes one of the user's live tokens; `not_found` if there is none by that id.
pub async fn revoke_token(db: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<()> {
    let done = sqlx::query(
        "UPDATE tokens SET revoked_at = now() \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError(ErrorCode::NotFound));
    }
    Ok(())
}

/// The (user, token id, scope) of a live token, recording its use.
pub async fn resolve(db: &PgPool, secret: &str) -> AppResult<Option<(Uuid, Uuid, Scope)>> {
    if !well_formed(secret) {
        return Ok(None);
    }
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "UPDATE tokens SET last_used_at = now() WHERE hash = $1 AND revoked_at IS NULL \
         RETURNING user_id, id, scope",
    )
    .bind(&crypto::sha256(secret.as_bytes())[..])
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|(user, id, scope)| Scope::parse(&scope).map(|s| (user, id, s))))
}

#[derive(Deserialize)]
pub struct CreateRequest {
    pub name: String,
    pub scope: String,
}

/// `POST /tokens` (session + CSRF header).
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CreateRequest>,
) -> AppResult<impl IntoResponse> {
    caller.session_write(&headers)?;
    let scope = Scope::parse(&body.scope).ok_or(AppError(ErrorCode::InvalidScope))?;
    let (info, secret) = create_token(&state.db, caller.user_id, &body.name, scope).await?;
    let mut value = serde_json::to_value(&info).map_err(AppError::internal)?;
    value["secret"] = serde_json::Value::String(secret);
    Ok((StatusCode::CREATED, Json(value)))
}

/// `GET /tokens` (session).
pub async fn list(State(state): State<AppState>, caller: Caller) -> AppResult<impl IntoResponse> {
    caller.require_session()?;
    let tokens = list_tokens(&state.db, caller.user_id).await?;
    Ok(Json(serde_json::json!({ "tokens": tokens })))
}

/// `DELETE /tokens/{id}` (session + CSRF header).
pub async fn revoke(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    ApiPath(id): ApiPath<Uuid>,
) -> AppResult<StatusCode> {
    caller.session_write(&headers)?;
    revoke_token(&state.db, caller.user_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_have_the_documented_shape() {
        let secret = new_secret();
        assert_eq!(secret.len(), 4 + 43);
        assert!(secret.starts_with("kfm_"));
        assert!(well_formed(&secret));
        assert!(!well_formed("kfm_short"));
        assert!(!well_formed(&secret[4..]));
        assert_ne!(new_secret(), secret);
    }

    #[test]
    fn submit_covers_read() {
        assert!(Scope::Submit.covers(Scope::Read));
        assert!(Scope::Submit.covers(Scope::Submit));
        assert!(Scope::Read.covers(Scope::Read));
        assert!(!Scope::Read.covers(Scope::Submit));
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(clean_name("  laptop ").unwrap(), "laptop");
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(65)).is_err());
        assert!(clean_name("a\nb").is_err());
    }
}
