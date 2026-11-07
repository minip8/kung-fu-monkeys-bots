//! Seasons: the operator's `season create` and `season list`, the windows
//! that never overlap (so at most one season is current), and `GET /seasons`.

mod common;

use common::start;
use kfm_ladder::seasons::{self, SeasonError};
use reqwest::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

fn at(text: &str) -> OffsetDateTime {
    seasons::parse_time(text).unwrap()
}

#[sqlx::test]
async fn windows_never_overlap_so_one_season_at_most_is_current(pool: PgPool) {
    let now = OffsetDateTime::now_utc();
    let past = seasons::create(
        &pool,
        "past",
        now - Duration::days(60),
        now - Duration::days(30),
        "contest",
    )
    .await
    .unwrap();
    let current = seasons::create(
        &pool,
        "now",
        now - Duration::days(30),
        now + Duration::days(1),
        "contest",
    )
    .await
    .unwrap();
    // Touching windows are fine ([start, end)); overlapping ones are not.
    let next = seasons::create(
        &pool,
        "next",
        now + Duration::days(1),
        now + Duration::days(31),
        "contest",
    )
    .await
    .unwrap();
    match seasons::create(
        &pool,
        "overlap",
        now - Duration::days(40),
        now - Duration::days(29),
        "contest",
    )
    .await
    {
        Err(SeasonError::Overlaps(names)) => assert_eq!(names, ["past", "now"]),
        other => panic!("{other:?}"),
    }
    // The constraint holds without the service's check too.
    let raw = sqlx::query(
        "INSERT INTO seasons (id, name, starts_at, ends_at) VALUES (gen_random_uuid(), 'raw', $1, $2)",
    )
    .bind(now)
    .bind(now + Duration::hours(1))
    .execute(&pool)
    .await
    .unwrap_err();
    let sqlx::Error::Database(e) = raw else {
        panic!("{raw:?}")
    };
    assert_eq!(e.constraint(), Some("seasons_no_overlap"));

    assert!(matches!(
        seasons::create(
            &pool,
            "now",
            now + Duration::days(100),
            now + Duration::days(101),
            "contest"
        )
        .await,
        Err(SeasonError::NameTaken)
    ));
    assert!(matches!(
        seasons::create(
            &pool,
            "empty",
            now + Duration::days(200),
            now + Duration::days(200),
            "contest"
        )
        .await,
        Err(SeasonError::EmptyWindow)
    ));
    assert!(matches!(
        seasons::create(
            &pool,
            "m",
            now + Duration::days(300),
            now + Duration::days(301),
            "Contest!"
        )
        .await,
        Err(SeasonError::InvalidMode)
    ));

    assert_eq!(
        seasons::current(&pool).await.unwrap(),
        Some(current.clone())
    );
    assert_eq!(seasons::shown(&pool).await.unwrap(), Some(current.clone()));
    let names: Vec<String> = seasons::list(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["next", "now", "past"]);
    assert_eq!(
        seasons::by_name(&pool, "past").await.unwrap(),
        Some(past.clone())
    );
    let _ = next;
}

#[sqlx::test]
async fn between_seasons_none_is_current_and_the_last_one_is_shown(pool: PgPool) {
    assert_eq!(seasons::current(&pool).await.unwrap(), None);
    assert_eq!(seasons::shown(&pool).await.unwrap(), None);
    let now = OffsetDateTime::now_utc();
    let ended = seasons::create(
        &pool,
        "ended",
        now - Duration::days(10),
        now - Duration::days(1),
        "contest",
    )
    .await
    .unwrap();
    seasons::create(
        &pool,
        "later",
        now + Duration::days(1),
        now + Duration::days(2),
        "contest",
    )
    .await
    .unwrap();
    assert_eq!(seasons::current(&pool).await.unwrap(), None);
    assert_eq!(seasons::shown(&pool).await.unwrap(), Some(ended));
}

#[sqlx::test]
async fn get_seasons_lists_them_with_the_current_one_marked(pool: PgPool) {
    let app = start(pool.clone()).await;
    let now = OffsetDateTime::now_utc();
    seasons::create(
        &pool,
        "S1",
        now - Duration::days(1),
        now + Duration::days(1),
        "contest",
    )
    .await
    .unwrap();
    seasons::create(
        &pool,
        "S0",
        at("2026-01-01T00:00:00Z"),
        at("2026-02-01T00:00:00Z"),
        "grid_rounds_1t",
    )
    .await
    .unwrap();
    let response = reqwest::get(format!("{}/seasons", app.base)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    let list = body["seasons"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["name"], "S1");
    assert_eq!(list[0]["current"], true);
    assert_eq!(list[0]["mode"], "contest");
    assert_eq!(list[1]["name"], "S0");
    assert_eq!(list[1]["current"], false);
    assert_eq!(list[1]["mode"], "grid_rounds_1t");
    assert_eq!(list[1]["starts_at"], "2026-01-01T00:00:00Z");
    assert_eq!(list[1]["ends_at"], "2026-02-01T00:00:00Z");
    for key in ["id", "name", "mode", "starts_at", "ends_at", "current"] {
        assert!(list[0].get(key).is_some(), "{key}");
    }
    assert_eq!(list[0].as_object().unwrap().len(), 6);
}

/// The database URL of the test's own database (sqlx::test made it on the
/// server DATABASE_URL names).
async fn own_url(pool: &PgPool) -> String {
    let (name,): (String,) = sqlx::query_as("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(&format!("/{name}"));
    url.to_string()
}

#[sqlx::test]
async fn the_season_subcommand_creates_and_lists(pool: PgPool) {
    let url = own_url(&pool).await;
    let bin = env!("CARGO_BIN_EXE_kfm-ladder");
    let run = |args: &[&str]| {
        std::process::Command::new(bin)
            .args(args)
            .env("DATABASE_URL", &url)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap()
    };
    let now = OffsetDateTime::now_utc();
    let fmt = |t: OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let start = fmt(now - Duration::hours(1));
    let end = fmt(now + Duration::days(30));
    let created = run(&[
        "season", "create", "2026-11", "--start", &start, "--end", &end,
    ]);
    assert!(created.status.success(), "{created:?}");
    let line = String::from_utf8(created.stdout).unwrap();
    assert!(line.starts_with("2026-11\t"), "{line}");
    assert!(line.trim_end().ends_with("\tcurrent"), "{line}");
    assert!(line.contains("\tcontest\t"), "{line}");

    let overlap = run(&[
        "season", "create", "again", "--start", &start, "--end", &end,
    ]);
    assert_eq!(overlap.status.code(), Some(1));
    let err = String::from_utf8(overlap.stderr).unwrap();
    assert!(err.contains("overlaps the season(s) 2026-11"), "{err}");

    let bad = run(&[
        "season",
        "create",
        "x",
        "--start",
        "yesterday",
        "--end",
        &end,
    ]);
    assert_eq!(bad.status.code(), Some(2));

    let listed = run(&["season", "list"]);
    assert!(listed.status.success());
    let text = String::from_utf8(listed.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with("2026-11\t"));
}
