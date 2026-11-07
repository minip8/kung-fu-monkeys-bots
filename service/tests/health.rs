//! Migrations, `/healthz` and the error shape of unknown routes.

mod common;

use common::{error_code, start};
use reqwest::StatusCode;
use sqlx::PgPool;

const TABLES: [&str; 13] = [
    "users",
    "identities",
    "sessions",
    "tokens",
    "teams",
    "memberships",
    "submissions",
    "active_bots",
    "jobs",
    "matches",
    "ratings",
    "seasons",
    "rounds",
];

#[sqlx::test(migrations = false)]
async fn migrations_apply_and_rerun_cleanly(pool: PgPool) {
    kfm_ladder::db::migrate(&pool).await.unwrap();
    kfm_ladder::db::migrate(&pool).await.unwrap();
    for table in TABLES {
        let (exists,): (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(exists, "table {table}");
    }
    for index in [
        "identities_pkey",
        "tokens_hash",
        "jobs_state_kind",
        "memberships_pkey",
        "teams_name_lower",
    ] {
        let (exists,): (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{index}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(exists, "index {index}");
    }
}

#[sqlx::test]
async fn check_constraints_hold_the_enumerations(pool: PgPool) {
    let bad_job = sqlx::query("INSERT INTO jobs (id, kind) VALUES ($1, 'mine')")
        .bind(uuid::Uuid::now_v7())
        .execute(&pool)
        .await;
    assert!(bad_job.is_err());
    let good_job = sqlx::query("INSERT INTO jobs (id, kind) VALUES ($1, 'build') RETURNING state")
        .bind(uuid::Uuid::now_v7())
        .execute(&pool)
        .await;
    assert!(good_job.is_ok());
}

#[sqlx::test]
async fn healthz_reports_the_database(pool: PgPool) {
    let app = start(pool.clone()).await;
    let response = reqwest::get(format!("{}/healthz", app.base)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body, serde_json::json!({ "ok": true, "db": true }));

    pool.close().await;
    let response = reqwest::get(format!("{}/healthz", app.base)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body, serde_json::json!({ "ok": false, "db": false }));
}

#[sqlx::test]
async fn unknown_routes_and_methods_have_the_error_shape(pool: PgPool) {
    let app = start(pool).await;
    let http = reqwest::Client::new();
    let response = http.get(format!("{}/nope", app.base)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "not_found");
    let response = http
        .put(format!("{}/healthz", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(error_code(response).await, "method_not_allowed");
    let response = http
        .get(format!("{}/teams/not-a-uuid", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "not_found");
}
