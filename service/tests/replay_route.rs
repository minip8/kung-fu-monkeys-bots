//! `GET /matches/{m}/replay` (API v1.10): the game's `.kfmr` served by the
//! ladder, with `Access-Control-Allow-Origin: *` on every answer, so the
//! engine's page opens `?match=<id>`; and `viewer_url`'s short form for a
//! release that opens match links (`MATCH_VIEWER_SINCE`).

mod common;

use common::{db_ready_bot, db_season, db_team, start, start_configured, test_s3};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// A recorded game under `tag`, with its replay key (the object is the
/// caller's business).
async fn db_match(pool: &PgPool, tag: &str) -> (Uuid, String) {
    let season = db_season(pool, "S", 30).await;
    let team_a = db_team(pool, "Alpha").await;
    let team_b = db_team(pool, "Beta").await;
    let sub_a = db_ready_bot(pool, None, team_a, b"\0asm a").await;
    let sub_b = db_ready_bot(pool, None, team_b, b"\0asm b").await;
    let id = Uuid::now_v7();
    let key = kfm_ladder::match_worker::replay_key(id);
    sqlx::query(
        "INSERT INTO matches (id, season_id, seed, engine_tag, content_hash, build_hash, team_a, submission_a, \
           team_b, submission_b, winner_team, replay_key, result, round, game, side_a, ending, ticks, \
           score_a, kills_a, deaths_a, faults_a, delta_a, score_b, kills_b, deaths_b, faults_b, delta_b) \
         VALUES ($1, $2, 42, $3, 'a99dfcf8', '00000000', $4, $5, $6, $7, $4, $8, $9, 1, 0, 0, \
           'ended', 2000, 3, 9, 2, '[]', 12.5, 1, 2, 9, '[]', -12.5)",
    )
    .bind(id)
    .bind(season.id)
    .bind(tag)
    .bind(team_a)
    .bind(sub_a)
    .bind(team_b)
    .bind(sub_b)
    .bind(&key)
    .bind(json!({"match": 0, "seed": 42, "checksum": "0badf00d"}))
    .execute(pool)
    .await
    .unwrap();
    (id, key)
}

fn allows_any_origin(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("access-control-allow-origin")
        .is_some_and(|v| v == "*")
}

async fn not_found(response: reqwest::Response) {
    assert_eq!(response.status(), 404);
    assert!(allows_any_origin(&response));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "not_found");
}

#[sqlx::test]
async fn the_replay_is_served_by_match_id_with_cors(pool: PgPool) {
    if test_s3().is_none() {
        eprintln!("skipped: KFM_LADDER_TEST_S3_ENDPOINT is unset");
        return;
    }
    let app = start(pool.clone()).await;
    let (id, key) = db_match(&pool, "v0.11.0").await;
    let bytes: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    app.store
        .put(&key, &bytes, "application/octet-stream")
        .await
        .unwrap();

    let response = reqwest::get(format!("{}/matches/{id}/replay", app.base))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(allows_any_origin(&response));
    let headers = response.headers();
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["cache-control"], "public, max-age=3600");
    assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);

    // The match's viewer link is the short form for this release.
    let detail: Value = reqwest::get(format!("{}/matches/{id}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["viewer_url"],
        format!("https://kfm.minip8.com/v0.11.0/?match={id}")
    );
    assert!(detail["replay_url"].as_str().unwrap().contains("X-Amz-"));

    // The object gone from the store (pruned under the row's feet): 404.
    app.store.delete(&key).await.unwrap();
    not_found(
        reqwest::get(format!("{}/matches/{id}/replay", app.base))
            .await
            .unwrap(),
    )
    .await;
}

#[sqlx::test]
async fn a_pruned_or_unknown_replay_is_not_found_with_cors(pool: PgPool) {
    let app = start(pool.clone()).await;
    let (id, _) = db_match(&pool, "v0.11.0").await;
    sqlx::query("UPDATE matches SET replay_key = NULL WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    not_found(
        reqwest::get(format!("{}/matches/{id}/replay", app.base))
            .await
            .unwrap(),
    )
    .await;
    // Pruned: no viewer link at all.
    let detail: Value = reqwest::get(format!("{}/matches/{id}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["viewer_url"], Value::Null);
    for path in [
        format!("/matches/{}/replay", Uuid::now_v7()),
        "/matches/not-a-uuid/replay".to_owned(),
    ] {
        not_found(reqwest::get(format!("{}{path}", app.base)).await.unwrap()).await;
    }
}

#[sqlx::test]
async fn an_older_release_keeps_the_presigned_viewer_link(pool: PgPool) {
    let app = start(pool.clone()).await;
    let (id, _) = db_match(&pool, "v0.10.0").await;
    let detail: Value = reqwest::get(format!("{}/matches/{id}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay = detail["replay_url"].as_str().unwrap();
    let encoded: String = url::form_urlencoded::byte_serialize(replay.as_bytes()).collect();
    assert_eq!(
        detail["viewer_url"],
        format!("https://kfm.minip8.com/v0.10.0/?replay={encoded}")
    );
}

#[sqlx::test]
async fn the_replay_route_counts_against_the_presigning_limit(pool: PgPool) {
    let app = start_configured(pool.clone(), |c| c.presign_per_minute = 1).await;
    let id = Uuid::now_v7();
    let url = format!("{}/matches/{id}/replay", app.base);
    not_found(reqwest::get(&url).await.unwrap()).await;
    let limited = reqwest::get(&url).await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(allows_any_origin(&limited));
    assert!(limited.headers().contains_key("retry-after"));
}
