//! Replay retention (API v1.4): a game's replay object goes once the game is
//! `KFM_LADDER_REPLAY_HOURS` old, its key is cleared and `GET /matches/{m}`
//! answers `replay_url: null`; the row keeps the rest. The sweep runs in the
//! scheduler's loop under its lock; 0 hours turns it off; a store failure
//! leaves the key for the next sweep.

mod common;

use std::time::Duration;

use common::{db_ready_bot, db_season, db_team, start, test_s3, test_store};
use kfm_ladder::config::S3Config;
use kfm_ladder::retention::{self, ReplayPrune, REPLAY_BATCH};
use kfm_ladder::scheduler::{self, SchedulerSettings};
use kfm_ladder::Store;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::watch;
use uuid::Uuid;

const DAY: Duration = Duration::from_secs(24 * 3600);

struct Pair {
    season: Uuid,
    a: (Uuid, Uuid),
    b: (Uuid, Uuid),
}

async fn pair(pool: &PgPool) -> Pair {
    let season = db_season(pool, "S", 30).await;
    let team_a = db_team(pool, "Alpha").await;
    let team_b = db_team(pool, "Beta").await;
    let sub_a = db_ready_bot(pool, None, team_a, b"\0asm a").await;
    let sub_b = db_ready_bot(pool, None, team_b, b"\0asm b").await;
    Pair {
        season: season.id,
        a: (team_a, sub_a),
        b: (team_b, sub_b),
    }
}

/// A recorded game `minutes` ago with its replay key (the object is the
/// caller's business).
async fn db_match(pool: &PgPool, pair: &Pair, minutes: i64) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let key = kfm_ladder::match_worker::replay_key(id);
    sqlx::query(
        "INSERT INTO matches (id, season_id, seed, engine_tag, content_hash, build_hash, team_a, submission_a, \
           team_b, submission_b, winner_team, replay_key, result, round, game, side_a, ending, ticks, \
           score_a, kills_a, deaths_a, faults_a, delta_a, score_b, kills_b, deaths_b, faults_b, delta_b, created_at) \
         VALUES ($1, $2, 42, 'v0.5.1', 'a99dfcf8', '00000000', $3, $4, $5, $6, $3, $7, $8, 1, 0, 0, \
           'ended', 2000, 3, 9, 2, '[]', 12.5, 1, 2, 9, '[]', -12.5, now() - make_interval(mins => $9))",
    )
    .bind(id)
    .bind(pair.season)
    .bind(pair.a.0)
    .bind(pair.a.1)
    .bind(pair.b.0)
    .bind(pair.b.1)
    .bind(&key)
    .bind(json!({"match": 0, "seed": 42, "checksum": "0badf00d"}))
    .bind(i32::try_from(minutes).unwrap())
    .execute(pool)
    .await
    .unwrap();
    (id, key)
}

async fn key_of(pool: &PgPool, id: Uuid) -> Option<String> {
    let (key,): (Option<String>,) = sqlx::query_as("SELECT replay_key FROM matches WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    key
}

async fn prune(pool: &PgPool, store: &Store, older_than: Duration) -> ReplayPrune {
    let mut conn = pool.acquire().await.unwrap();
    retention::prune_replays(&mut conn, store, older_than)
        .await
        .unwrap()
}

/// A store nothing answers at: every request fails.
fn dead_store() -> Store {
    Store::new(&S3Config {
        endpoint: "http://127.0.0.1:9".parse().unwrap(),
        bucket: "kfm".to_owned(),
        access_key: "ak".to_owned(),
        secret_key: "sk".to_owned(),
        region: "auto".to_owned(),
    })
    .unwrap()
}

#[sqlx::test]
async fn only_old_replays_go_and_the_match_keeps_the_rest(pool: PgPool) {
    let Some(store) = test_store("only_old_replays_go_and_the_match_keeps_the_rest").await else {
        return;
    };
    let pair = pair(&pool).await;
    let (old, old_key) = db_match(&pool, &pair, 25 * 60).await;
    let (older, older_key) = db_match(&pool, &pair, 48 * 60).await;
    let (fresh, fresh_key) = db_match(&pool, &pair, 23 * 60).await;
    for key in [&old_key, &older_key, &fresh_key] {
        store
            .put(key, b"KFMR replay", "application/octet-stream")
            .await
            .unwrap();
    }
    // More old games than a batch, without objects: a missing object counts
    // as deleted, and the sweep walks every batch.
    let mut missing = Vec::new();
    for i in 0..(REPLAY_BATCH + 5) {
        missing.push(db_match(&pool, &pair, 30 * 60 + i).await.0);
    }

    let done = prune(&pool, &store, DAY).await;
    assert_eq!(
        done,
        ReplayPrune {
            deleted: REPLAY_BATCH as u64 + 5 + 2,
            failed: 0
        }
    );
    for id in [old, older].iter().chain(&missing) {
        assert_eq!(key_of(&pool, *id).await, None);
    }
    assert_eq!(key_of(&pool, fresh).await.as_deref(), Some(&fresh_key[..]));
    assert_eq!(store.get(&old_key).await.unwrap(), None);
    assert_eq!(store.get(&older_key).await.unwrap(), None);
    assert!(store.get(&fresh_key).await.unwrap().is_some());
    // A second sweep finds nothing.
    assert_eq!(prune(&pool, &store, DAY).await, ReplayPrune::default());

    // The route answers null links for a pruned game, the rest unchanged.
    let app = start(pool.clone()).await;
    let detail: Value = reqwest::get(format!("{}/matches/{old}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["replay_url"], Value::Null);
    assert_eq!(detail["viewer_url"], Value::Null);
    assert_eq!(detail["seed"], 42);
    assert_eq!(detail["result"]["checksum"], "0badf00d");
    assert_eq!(detail["winner"], pair.a.0.to_string());
    assert_eq!(detail["sides"][0]["submission_id"], pair.a.1.to_string());
    assert_eq!(detail["sides"][1]["submission_id"], pair.b.1.to_string());
    assert_eq!(detail["sides"][0]["rating_change"], 12.5);
    let html = reqwest::Client::new()
        .get(format!("{}/matches/{old}", app.base))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("?replay="), "{html}");
    let detail: Value = reqwest::get(format!("{}/matches/{fresh}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(detail["replay_url"].is_string());
    assert!(detail["viewer_url"].is_string());
    store.delete(&fresh_key).await.unwrap();
}

#[sqlx::test]
async fn a_store_failure_leaves_the_key_for_the_next_sweep(pool: PgPool) {
    let pair = pair(&pool).await;
    let (old, key) = db_match(&pool, &pair, 25 * 60).await;
    let done = prune(&pool, &dead_store(), DAY).await;
    assert_eq!(
        done,
        ReplayPrune {
            deleted: 0,
            failed: 1
        }
    );
    assert_eq!(key_of(&pool, old).await.as_deref(), Some(&key[..]));
    // The next sweep, with the store back, takes it.
    if let Some(store) = test_store("a_store_failure_leaves_the_key_for_the_next_sweep").await {
        assert_eq!(prune(&pool, &store, DAY).await.deleted, 1);
        assert_eq!(key_of(&pool, old).await, None);
    }
}

/// Runs the scheduler loop with `hours` of replay retention until `done`
/// holds or `wait` passes, then stops it.
async fn run_loop(
    pool: &PgPool,
    store: Store,
    hours: u64,
    wait: Duration,
    done: impl AsyncFn() -> bool,
) {
    let settings =
        SchedulerSettings::new(Duration::from_secs(3600), 3).with_replay_hours(store, hours);
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(scheduler::run(pool.clone(), settings, stopped));
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline && !done().await {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
}

#[sqlx::test]
async fn the_scheduler_sweeps_and_zero_hours_keeps_replays(pool: PgPool) {
    let pair = pair(&pool).await;
    let (old, key) = db_match(&pool, &pair, 25 * 60).await;
    // 0: no sweep at all, even with a store that works (or one that fails,
    // which would leave the key anyway: the off switch is the settings').
    assert!(SchedulerSettings::new(Duration::from_secs(60), 3)
        .with_replay_hours(dead_store(), 0)
        .replays
        .is_none());
    let Some(config) = test_s3() else {
        eprintln!("the_scheduler_sweeps_and_zero_hours_keeps_replays: skipped: KFM_LADDER_TEST_S3_ENDPOINT is unset");
        return;
    };
    let store = Store::new(&config).unwrap();
    store.ensure_bucket().await.unwrap();
    store
        .put(&key, b"KFMR", "application/octet-stream")
        .await
        .unwrap();
    run_loop(
        &pool,
        store.clone(),
        0,
        Duration::from_millis(600),
        async || false,
    )
    .await;
    assert_eq!(key_of(&pool, old).await.as_deref(), Some(&key[..]));
    assert!(store.get(&key).await.unwrap().is_some());
    // 24: the lock holder sweeps at once.
    run_loop(
        &pool,
        store.clone(),
        24,
        Duration::from_secs(10),
        async || key_of(&pool, old).await.is_none(),
    )
    .await;
    assert_eq!(key_of(&pool, old).await, None);
    assert_eq!(store.get(&key).await.unwrap(), None);
}

#[sqlx::test]
async fn an_instance_without_the_lock_never_sweeps(pool: PgPool) {
    let Some(store) = test_store("an_instance_without_the_lock_never_sweeps").await else {
        return;
    };
    let pair = pair(&pool).await;
    let (old, key) = db_match(&pool, &pair, 25 * 60).await;
    store
        .put(&key, b"KFMR", "application/octet-stream")
        .await
        .unwrap();
    let mut other = pool.acquire().await.unwrap().detach();
    assert!(scheduler::try_lock(&mut other).await.unwrap());
    run_loop(
        &pool,
        store.clone(),
        24,
        Duration::from_millis(600),
        async || false,
    )
    .await;
    assert_eq!(key_of(&pool, old).await.as_deref(), Some(&key[..]));
    assert!(scheduler::unlock(&mut other).await.unwrap());
    store.delete(&key).await.unwrap();
}
