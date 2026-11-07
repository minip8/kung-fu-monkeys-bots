//! Submissions (`docs/ladder-api.md` §9): a team's source archive and
//! manifest in, a `build` job queued, the module and its log out once a build
//! worker is done; activation of a ready one.
//!
//! States: `queued` (uploaded, waiting for a build worker) → `building` (a
//! worker claimed its build) → `failed` (the build did not produce a module),
//! `rejected` (the module fails validation or the smoke match) or `ready`.
//! Only `ready` may be made active.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Multipart, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, AppError, AppResult, ErrorCode};
use crate::extract::{ApiPath, JsonBody};
use crate::tokens::Scope;
use crate::{archive, jobs, manifest, store, AppState};

/// How long a team waits between two submissions.
pub const SUBMIT_INTERVAL_SECS: i32 = 60;
/// How long a presigned log URL stays valid.
pub const LOG_URL_TTL: Duration = Duration::from_secs(15 * 60);
/// The request body cap of an upload: the archive's cap plus room for the
/// manifest and the multipart framing.
pub const MAX_UPLOAD_BYTES: usize = archive::MAX_ZIP_BYTES + 64 * 1024;
/// The multipart part names.
pub const MANIFEST_PART: &str = "manifest.toml";
pub const SOURCE_PART: &str = "source.zip";

/// A submission's row, with whether it is its team's active one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubmissionRow {
    pub id: Uuid,
    pub team_id: Uuid,
    pub submitted_by: Option<Uuid>,
    pub manifest: String,
    pub source_key: Option<String>,
    pub module_key: Option<String>,
    pub module_hash: Option<Vec<u8>>,
    pub log_key: Option<String>,
    pub state: String,
    pub error: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub ranked_games: i32,
    pub ranked_faults: i32,
    pub active: bool,
}

/// A submission as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Submission {
    pub id: Uuid,
    pub team_id: Uuid,
    pub submitted_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub state: String,
    pub language: String,
    pub entry: String,
    pub module_hash: Option<String>,
    pub log_url: Option<String>,
    pub error: Option<String>,
    pub active: bool,
    /// Ranked games it played (v1.2).
    pub ranked_games: i32,
    /// Of those, the games in which one of its slots faulted (v1.2).
    pub ranked_faults: i32,
}

/// The columns of [`SubmissionRow`], `s` the submission and `a` its team's
/// `active_bots` row joined on the submission.
pub const SELECT_ROW: &str = "SELECT s.id, s.team_id, s.submitted_by, s.manifest, s.source_key, \
     s.module_key, s.module_hash, s.log_key, s.state, s.error, s.created_at, s.updated_at, \
     s.ranked_games, s.ranked_faults, (a.submission_id IS NOT NULL) AS active \
     FROM submissions s LEFT JOIN active_bots a ON a.team_id = s.team_id AND a.submission_id = s.id";

pub async fn get_row(db: &PgPool, id: Uuid) -> AppResult<Option<SubmissionRow>> {
    Ok(sqlx::query_as(&format!("{SELECT_ROW} WHERE s.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// The view of a row: the manifest's fields, the hash in hex and a fresh
/// presigned URL for the log when one is stored.
pub async fn view(state: &AppState, row: SubmissionRow) -> AppResult<Submission> {
    let (language, entry) = match manifest::parse(&row.manifest) {
        Ok(m) => (m.language.as_str().to_owned(), m.entry_display().to_owned()),
        // A stored manifest always parsed once; show it raw if a rule changed.
        Err(_) => (String::new(), String::new()),
    };
    let log_url = match &row.log_key {
        Some(key) => Some(
            state
                .store
                .presign_get(key, LOG_URL_TTL)
                .await
                .map_err(AppError::internal)?,
        ),
        None => None,
    };
    Ok(Submission {
        id: row.id,
        team_id: row.team_id,
        submitted_by: row.submitted_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        state: row.state,
        language,
        entry,
        module_hash: row.module_hash.as_deref().map(hex::encode),
        log_url,
        error: row.error,
        active: row.active,
        ranked_games: row.ranked_games,
        ranked_faults: row.ranked_faults,
    })
}

/// The caller's role in `team`: `not_found` without the team, `forbidden`
/// for a non-member.
async fn member_role(db: &PgPool, team: Uuid, user: Uuid) -> AppResult<String> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT m.role FROM teams t LEFT JOIN memberships m \
         ON m.team_id = t.id AND m.user_id = $2 WHERE t.id = $1",
    )
    .bind(team)
    .bind(user)
    .fetch_optional(db)
    .await?;
    match row {
        None => Err(AppError(ErrorCode::NotFound)),
        Some((None,)) => Err(AppError(ErrorCode::Forbidden)),
        Some((Some(role),)) => Ok(role),
    }
}

/// Locks the team's row (a team's submissions and activations serialise on
/// it) and checks `user` is a member, as [`member_role`].
async fn lock_member(tx: &mut Transaction<'_, Postgres>, team: Uuid, user: Uuid) -> AppResult<()> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT (SELECT role FROM memberships WHERE team_id = t.id AND user_id = $2) \
         FROM teams t WHERE t.id = $1 FOR UPDATE",
    )
    .bind(team)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        None => Err(AppError(ErrorCode::NotFound)),
        Some((None,)) => Err(AppError(ErrorCode::Forbidden)),
        Some((Some(_),)) => Ok(()),
    }
}

/// Checks an upload and queues its build: the manifest parsed, the archive
/// walked (every rule of [`archive::check`], and the entry holding sources),
/// the team's rate checked, the zip stored, the row and its `build` job
/// inserted. Nothing is stored for a refused upload.
pub async fn create_submission(
    state: &AppState,
    team: Uuid,
    user: Uuid,
    manifest_text: &str,
    zip: &[u8],
) -> ApiResult<Submission> {
    member_role(&state.db, team, user).await?;
    let manifest = manifest::parse(manifest_text)
        .map_err(|detail| ApiError::with_detail(ErrorCode::BadManifest, detail))?;
    if zip.len() > archive::MAX_ZIP_BYTES {
        return Err(ErrorCode::TooLarge.into());
    }
    let contents = archive::check(zip)
        .map_err(|detail| ApiError::with_detail(ErrorCode::BadSource, detail))?;
    contents
        .check_entry(&manifest.entry)
        .map_err(|detail| ApiError::with_detail(ErrorCode::BadSource, detail))?;

    let mut tx = state.db.begin().await?;
    lock_member(&mut tx, team, user).await?;
    let (recent,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM submissions WHERE team_id = $1 \
         AND created_at > now() - make_interval(secs => $2))",
    )
    .bind(team)
    .bind(f64::from(SUBMIT_INTERVAL_SECS))
    .fetch_one(&mut *tx)
    .await?;
    if recent {
        return Err(ErrorCode::RateLimited.into());
    }
    let id = Uuid::now_v7();
    let source_key = store::source_key(id);
    state
        .store
        .put(&source_key, zip, "application/zip")
        .await
        .map_err(AppError::internal)?;
    sqlx::query(
        "INSERT INTO submissions (id, team_id, submitted_by, manifest, source_key) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(team)
    .bind(user)
    .bind(manifest_text)
    .bind(&source_key)
    .execute(&mut *tx)
    .await?;
    jobs::enqueue(
        &mut tx,
        jobs::Kind::Build,
        serde_json::json!({ "submission": id }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(submission = %id, %team, files = contents.files.len(), "submission queued");
    let row = get_row(&state.db, id)
        .await?
        .ok_or_else(|| AppError::internal("a submission vanished after its insert"))?;
    Ok(view(state, row).await?)
}

/// The team's submissions that retention keeps, newest first: every one
/// whose source is still stored (the five most recent finished ones and
/// those still queued or building) and the active one.
pub async fn list_rows(db: &PgPool, team: Uuid) -> AppResult<Vec<SubmissionRow>> {
    Ok(sqlx::query_as(&format!(
        "{SELECT_ROW} WHERE s.team_id = $1 AND (s.source_key IS NOT NULL OR a.submission_id IS NOT NULL) \
         ORDER BY s.created_at DESC, s.id DESC"
    ))
    .bind(team)
    .fetch_all(db)
    .await?)
}

pub async fn list_submissions(
    state: &AppState,
    team: Uuid,
    user: Uuid,
) -> AppResult<Vec<Submission>> {
    member_role(&state.db, team, user).await?;
    let mut out = Vec::new();
    for row in list_rows(&state.db, team).await? {
        out.push(view(state, row).await?);
    }
    Ok(out)
}

/// A submission the caller may see (a member of its team), else `not_found`.
pub async fn visible_row(db: &PgPool, id: Uuid, user: Uuid) -> AppResult<SubmissionRow> {
    let row = get_row(db, id)
        .await?
        .ok_or(AppError(ErrorCode::NotFound))?;
    match member_role(db, row.team_id, user).await {
        Ok(_) => Ok(row),
        Err(_) => Err(AppError(ErrorCode::NotFound)),
    }
}

/// Makes a ready submission of `team` its active one.
pub async fn activate(
    state: &AppState,
    team: Uuid,
    user: Uuid,
    submission: Uuid,
) -> AppResult<Submission> {
    let mut tx = state.db.begin().await?;
    lock_member(&mut tx, team, user).await?;
    let found: Option<(String,)> =
        sqlx::query_as("SELECT state FROM submissions WHERE id = $1 AND team_id = $2")
            .bind(submission)
            .bind(team)
            .fetch_optional(&mut *tx)
            .await?;
    match found {
        None => return Err(AppError(ErrorCode::NotFound)),
        Some((s,)) if s != "ready" => return Err(AppError(ErrorCode::NotReady)),
        Some(_) => {}
    }
    sqlx::query(
        "INSERT INTO active_bots (team_id, submission_id, activated_by) VALUES ($1, $2, $3) \
         ON CONFLICT (team_id) DO UPDATE SET submission_id = EXCLUDED.submission_id, \
         activated_by = EXCLUDED.activated_by, activated_at = now()",
    )
    .bind(team)
    .bind(submission)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(%team, %submission, "submission activated");
    let row = get_row(&state.db, submission)
        .await?
        .ok_or(AppError(ErrorCode::NotFound))?;
    view(state, row).await
}

/// The team's active submission, if any.
pub async fn active_of(db: &PgPool, team: Uuid) -> AppResult<Option<Uuid>> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT submission_id FROM active_bots WHERE team_id = $1")
            .bind(team)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(id,)| id))
}

/// The two parts of an upload, read with their caps.
pub struct Upload {
    pub manifest: Option<String>,
    pub zip: Option<Vec<u8>>,
    /// Other text fields (the portal's `csrf`, `language`, `entry`).
    pub fields: Vec<(String, String)>,
}

/// The largest plain form field the portal's upload form sends.
const MAX_FIELD_BYTES: usize = 1024;

fn multipart_error(err: &axum::extract::multipart::MultipartError) -> ApiError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ErrorCode::TooLarge.into()
    } else {
        ErrorCode::BadRequest.into()
    }
}

/// Reads a multipart upload: `manifest.toml` (at most 4 KiB), `source.zip`
/// (at most 4 MiB, else `too_large`) and small text fields; any part twice
/// is `bad_request`.
pub async fn read_upload(mut multipart: Multipart) -> ApiResult<Upload> {
    let mut upload = Upload {
        manifest: None,
        zip: None,
        fields: Vec::new(),
    };
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(&e))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let cap = match name.as_str() {
            SOURCE_PART => archive::MAX_ZIP_BYTES,
            MANIFEST_PART => manifest::MAX_MANIFEST_BYTES,
            _ => MAX_FIELD_BYTES,
        };
        let mut data = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
            if data.len() + chunk.len() > cap {
                return Err(match name.as_str() {
                    SOURCE_PART => ErrorCode::TooLarge.into(),
                    MANIFEST_PART => ApiError::with_detail(
                        ErrorCode::BadManifest,
                        format!(
                            "manifest.toml is over {} bytes",
                            manifest::MAX_MANIFEST_BYTES
                        ),
                    ),
                    _ => ErrorCode::BadRequest.into(),
                });
            }
            data.extend_from_slice(&chunk);
        }
        match name.as_str() {
            SOURCE_PART if upload.zip.is_none() => upload.zip = Some(data),
            MANIFEST_PART if upload.manifest.is_none() => {
                upload.manifest = Some(String::from_utf8(data).map_err(|_| {
                    ApiError::with_detail(ErrorCode::BadManifest, "manifest.toml is not UTF-8")
                })?);
            }
            SOURCE_PART | MANIFEST_PART => return Err(ErrorCode::BadRequest.into()),
            _ => {
                let text = String::from_utf8(data).map_err(|_| ErrorCode::BadRequest)?;
                upload.fields.push((name, text));
            }
        }
    }
    Ok(upload)
}

/// `POST /teams/{t}/submissions` (submit; a member): multipart with
/// `manifest.toml` and `source.zip`. `202` with the queued submission.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    ApiPath(team): ApiPath<Uuid>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> ApiResult<(StatusCode, Json<Submission>)> {
    caller.write_with_scope(&headers, Scope::Submit)?;
    let multipart = multipart.map_err(|_| ApiError::from(ErrorCode::BadRequest))?;
    let upload = read_upload(multipart).await?;
    let manifest = upload.manifest.ok_or_else(|| {
        ApiError::with_detail(ErrorCode::BadManifest, "the part manifest.toml is missing")
    })?;
    let zip = upload.zip.ok_or_else(|| {
        ApiError::with_detail(ErrorCode::BadSource, "the part source.zip is missing")
    })?;
    let submission = create_submission(&state, team, caller.user_id, &manifest, &zip).await?;
    Ok((StatusCode::ACCEPTED, Json(submission)))
}

/// `GET /teams/{t}/submissions` (any; a member).
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(team): ApiPath<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    caller.require_scope(Scope::Read)?;
    let submissions = list_submissions(&state, team, caller.user_id).await?;
    Ok(Json(serde_json::json!({ "submissions": submissions })))
}

/// `GET /submissions/{s}` (any; a member of its team).
pub async fn show(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<Uuid>,
) -> AppResult<Json<Submission>> {
    caller.require_scope(Scope::Read)?;
    let row = visible_row(&state.db, id, caller.user_id).await?;
    Ok(Json(view(&state, row).await?))
}

/// `GET /submissions/{s}/log` (any; a member of its team): the build and
/// validation log as text; `not_found` while there is none (queued,
/// building) or once retention pruned it.
pub async fn log(
    State(state): State<AppState>,
    caller: Caller,
    ApiPath(id): ApiPath<Uuid>,
) -> AppResult<Response> {
    caller.require_scope(Scope::Read)?;
    let row = visible_row(&state.db, id, caller.user_id).await?;
    let key = row.log_key.ok_or(AppError(ErrorCode::NotFound))?;
    let bytes = state
        .store
        .get(&key)
        .await
        .map_err(AppError::internal)?
        .ok_or(AppError(ErrorCode::NotFound))?;
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response.into_response())
}

#[derive(Deserialize)]
pub struct ActivateRequest {
    pub submission: Uuid,
}

/// `POST /teams/{t}/active` (submit; a member): `{"submission":"<id>"}`.
pub async fn activate_route(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    ApiPath(team): ApiPath<Uuid>,
    JsonBody(body): JsonBody<ActivateRequest>,
) -> AppResult<Json<Submission>> {
    caller.write_with_scope(&headers, Scope::Submit)?;
    Ok(Json(
        activate(&state, team, caller.user_id, body.submission).await?,
    ))
}
