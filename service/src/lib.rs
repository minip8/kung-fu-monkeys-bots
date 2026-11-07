//! The Kung Fu Monkeys ladder service (architecture v0.63 §11, "M7 step 3
//! design"; §8 "The ladder"). Step 3a: accounts through GitHub and Google
//! OAuth, portal sessions, API tokens and teams, over Postgres. The HTTP
//! surface is normative in `docs/ladder-api.md`.
//!
//! The binary is a thin CLI over [`serve`]; tests build an [`AppState`] with a
//! [`Config`] of their own, bind a listener on port 0 and pass both in.

pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod extract;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sqlx::PgPool;
use tokio::net::TcpListener;

pub use crate::config::Config;
use crate::error::{AppError, ErrorCode};

/// What every handler shares: the pool and the configuration.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> AppState {
        AppState {
            db,
            config: Arc::new(config),
        }
    }
}

/// Every route, without state (tests may add their own before `with_state`).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
}

/// The service's router with its state.
pub fn router(state: AppState) -> Router {
    routes().with_state(state)
}

/// Serves the API and the portal on `listener` until `shutdown` resolves.
pub async fn serve(
    state: AppState,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn not_found() -> AppError {
    AppError(ErrorCode::NotFound)
}

async fn method_not_allowed() -> AppError {
    AppError(ErrorCode::MethodNotAllowed)
}

/// `GET /healthz`: `{"ok":true,"db":true}` once `SELECT 1` answers, 503 with
/// `{"ok":false,"db":false}` otherwise.
async fn healthz(State(state): State<AppState>) -> Response {
    let db_ok = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(&state.db),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({ "ok": db_ok, "db": db_ok })),
    )
        .into_response()
}
