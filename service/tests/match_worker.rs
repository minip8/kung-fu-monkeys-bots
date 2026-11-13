//! The match job end to end with the released kfm_arena: a scheduler round's
//! pair played as one game on the arena and sides its seed draws, one row,
//! the replay in the store, the ratings moved, faults recorded (never a
//! rejection), the release widening, retries that record nothing twice, and
//! the jobs that record nothing.
//!
//! Needs Postgres (DATABASE_URL), a MinIO (KFM_LADDER_TEST_S3_ENDPOINT),
//! kfm_arena (KFM_LADDER_ARENA, default ../.kfm/kfm_arena) and the example
//! modules starter.wasm and chaser.wasm (KFM_LADDER_TEST_EXAMPLES, default
//! ../out/local: `tools/build.sh examples/<name> out/local/<name>.wasm`).
//! Each test says what is missing and passes without running when one is.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{db_ready_bot, db_season, db_team, have_store, start, TestApp};
use kfm_ladder::glicko::{self, Rating};
use kfm_ladder::match_worker::{draw, Draw, MatchSettings, MatchWorker};
use kfm_ladder::scheduler::{self, MatchPayload, RoundOutcome};
use kfm_ladder::worker::{self, Timing};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::watch;
use uuid::Uuid;

fn arena() -> PathBuf {
    std::env::var("KFM_LADDER_ARENA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.kfm/kfm_arena"))
        })
}

fn example(name: &str) -> PathBuf {
    let dir = std::env::var("KFM_LADDER_TEST_EXAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../out/local")));
    dir.join(format!("{name}.wasm"))
}

/// A module that passes every load check and traps on every act: what a
/// ranked bot that breaks under a new release looks like.
const TRAPPING: &str = "0061736d01000000010f036000017f60027e7f0060017f017e030504000001020503010001074205066d656d6f727902000f6b666d5f6162695f76657273696f6e00000e6b666d5f6f62735f6275666665720001086b666d5f696e69740002076b666d5f61637400030a1304040041010b05004180080b02000b0300000b";

/// The contest's arena pool in the pinned engine release (engine.lock,
/// v0.10.0), which the worker asks the runner for.
fn contest_pool() -> Vec<String> {
    ["grove", "orchards", "gorge", "burrows"]
        .map(str::to_owned)
        .to_vec()
}

/// What the worker draws for `seed`. A runner from before the `arena` key
/// names no pool, so only the side is drawn there.
fn drawn(seed: i64, raw: &Value) -> Draw {
    let pool = if raw.get("arena").is_some() {
        contest_pool()
    } else {
        Vec::new()
    };
    draw(u64::try_from(seed).unwrap(), &pool)
}

fn have_runner(test: &str) -> bool {
    if !have_store(test) {
        return false;
    }
    if !arena().is_file() {
        eprintln!(
            "{test}: skipped: no kfm_arena at {} (tools/fetch_kfm_arena.sh, or set KFM_LADDER_ARENA)",
            arena().display()
        );
        return false;
    }
    for name in ["starter", "chaser"] {
        if !example(name).is_file() {
            eprintln!(
                "{test}: skipped: no {} (tools/build.sh examples/{name} out/local/{name}.wasm, or set KFM_LADDER_TEST_EXAMPLES)",
                example(name).display()
            );
            return false;
        }
    }
    true
}

fn settings(ticks: u32) -> MatchSettings {
    MatchSettings {
        arena: arena(),
        engine_tag: "v0.5.1".to_owned(),
        ticks,
        work_dir: std::env::temp_dir().join(format!("kfm-ladder-match-test-{}", Uuid::now_v7())),
        timeout: Duration::from_secs(300),
    }
}

fn timing() -> Timing {
    Timing {
        poll: Duration::from_millis(50),
        heartbeat: Duration::from_secs(1),
        stale: Duration::from_secs(300),
        sweep_every: Duration::from_secs(60),
        retry_delay: Duration::ZERO,
    }
}

/// Runs a match worker until every match job is done or failed.
async fn work(app: &TestApp, settings: MatchSettings) {
    let dir = settings.work_dir.clone();
    let handler = MatchWorker::new(app.db.clone(), app.store.clone(), settings);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(worker::run(
        app.db.clone(),
        handler,
        worker::worker_name("test"),
        timing(),
        rx,
    ));
    for _ in 0..6000 {
        let (open,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM jobs WHERE kind = 'match' AND state IN ('queued', 'running')",
        )
        .fetch_one(&app.db)
        .await
        .unwrap();
        if open == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(300), task)
        .await
        .unwrap()
        .unwrap();
    // Every job directory was removed.
    if dir.exists() {
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

async fn one_round(pool: &PgPool) -> MatchPayload {
    let mut conn = pool.acquire().await.unwrap();
    let RoundOutcome::Ran { pairs, .. } =
        scheduler::run_round(&mut conn, Duration::from_secs(3600), 1)
            .await
            .unwrap()
    else {
        panic!("no round")
    };
    assert_eq!(pairs.len(), 1);
    let (payload,): (Value,) = sqlx::query_as("SELECT payload FROM jobs WHERE id = $1")
        .bind(pairs[0].job)
        .fetch_one(pool)
        .await
        .unwrap();
    serde_json::from_value(payload).unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Game {
    id: Uuid,
    game: i16,
    round: Option<i32>,
    seed: i64,
    engine_tag: String,
    content_hash: String,
    build_hash: Option<String>,
    team_a: Uuid,
    submission_a: Uuid,
    team_b: Uuid,
    submission_b: Uuid,
    side_a: i16,
    winner_team: Option<Uuid>,
    replay_key: Option<String>,
    result: Value,
    score_a: i32,
    kills_a: i32,
    score_b: i32,
    deaths_b: i32,
    faults_a: Value,
    faults_b: Value,
    delta_a: f64,
    delta_b: f64,
}

async fn games(pool: &PgPool) -> Vec<Game> {
    sqlx::query_as(
        "SELECT id, game, round, seed, engine_tag, content_hash, build_hash, team_a, submission_a, \
         team_b, submission_b, side_a, winner_team, replay_key, result, score_a, kills_a, score_b, \
         deaths_b, faults_a, faults_b, delta_a, delta_b FROM matches ORDER BY game",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct RatingRow {
    mu: f64,
    phi: f64,
    sigma: f64,
    games: i32,
    wins: i32,
    draws: i32,
    losses: i32,
    engine_tag: Option<String>,
}

async fn rating(pool: &PgPool, season: Uuid, team: Uuid) -> RatingRow {
    sqlx::query_as(
        "SELECT mu, phi, sigma, games, wins, draws, losses, engine_tag FROM ratings \
         WHERE season_id = $1 AND team_id = $2",
    )
    .bind(season)
    .bind(team)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_pair_plays_one_game_and_moves_the_ratings(pool: PgPool) {
    if !have_runner("a_pair_plays_one_game_and_moves_the_ratings") {
        return;
    }
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "S", 30).await;
    let starter = std::fs::read(example("starter")).unwrap();
    let chaser = std::fs::read(example("chaser")).unwrap();
    let t1 = db_team(&pool, "Starters").await;
    let s1 = db_ready_bot(&pool, Some(&app.store), t1, &starter).await;
    let t2 = db_team(&pool, "Chasers").await;
    let s2 = db_ready_bot(&pool, Some(&app.store), t2, &chaser).await;
    let payload = one_round(&pool).await;

    let started = std::time::Instant::now();
    work(&app, settings(36_000)).await;
    eprintln!(
        "a 5v5 contest game through the match worker: {:.2} s",
        started.elapsed().as_secs_f64()
    );
    let (state, error): (String, Option<String>) =
        sqlx::query_as("SELECT state, error FROM jobs WHERE kind = 'match'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), error), ("done", None));

    let rows = games(&pool).await;
    assert_eq!(rows.len(), 1, "a pair is one game");
    let teams = [payload.team_a, payload.team_b];
    assert!(teams.contains(&t1) && teams.contains(&t2));
    let row = &rows[0];
    assert_eq!(row.game, 0);
    assert_eq!(row.round, Some(1));
    assert_eq!(row.engine_tag, "v0.5.1");
    assert_eq!(row.seed, payload.seed);
    assert_eq!((row.team_a, row.team_b), (payload.team_a, payload.team_b));
    assert_eq!(
        (row.submission_a, row.submission_b),
        (payload.submission_a, payload.submission_b)
    );
    // The row agrees with the raw match object it came from, and both with
    // the draw of the job's seed: the arena, and the team side A plays.
    let raw = &row.result;
    let want = drawn(payload.seed, raw);
    assert_eq!(i16::from(want.side_a), row.side_a);
    assert_eq!(
        raw.get("arena").and_then(Value::as_str),
        want.arena.as_deref()
    );
    assert_eq!(raw["match"], 0);
    assert_eq!(raw["content_hash"], row.content_hash.as_str());
    assert_eq!(
        Some(raw["build_hash"].as_str().unwrap()),
        row.build_hash.as_deref()
    );
    let side = |entrant: u64| {
        raw["teams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["entrant"] == entrant)
            .unwrap()
            .clone()
    };
    // Without --rotate entrant e is the e-th --team and plays team e, so
    // side A is the entrant of its drawn team.
    let (a, b) = (u64::from(want.side_a), 1 - u64::from(want.side_a));
    assert_eq!(side(a)["team"], a);
    assert_eq!(side(a)["score"], row.score_a);
    assert_eq!(side(a)["kills"], row.kills_a);
    assert_eq!(side(b)["score"], row.score_b);
    assert_eq!(side(b)["deaths"], row.deaths_b);
    assert_eq!(raw["slots"].as_array().unwrap().len(), 10, "5v5");
    let winner = match raw["winner_team"].as_u64() {
        None => None,
        Some(t) if t == a => Some(row.team_a),
        Some(_) => Some(row.team_b),
    };
    assert_eq!(row.winner_team, winner);
    // The specs are the submissions' ids, not a worker's paths.
    for (entrant, submission) in [(a, row.submission_a), (b, row.submission_b)] {
        assert_eq!(
            side(entrant)["spec"].as_str().unwrap(),
            vec![format!("{submission}.wasm"); 5].join(",")
        );
    }
    // Clean modules: no fault.
    assert_eq!(row.faults_a, Value::Array(vec![]));
    assert_eq!(row.faults_b, Value::Array(vec![]));
    // The replay is in the store under the match's id.
    let key = row.replay_key.clone().unwrap();
    assert_eq!(key, format!("replays/{}.kfmr", row.id));
    let replay = app.store.get(&key).await.unwrap().expect("the replay");
    assert!(replay.len() > 1000, "{} bytes", replay.len());
    // Each side's change is its rating move.
    assert!(row.delta_a != 0.0 || row.winner_team.is_none());
    assert!(row.delta_a.signum() == -row.delta_b.signum() || row.delta_a == 0.0);
    for team in [t1, t2] {
        let r = rating(&pool, season.id, team).await;
        assert_eq!(r.games, 1);
        assert_eq!(r.wins + r.draws + r.losses, 1);
        assert_eq!(r.engine_tag.as_deref(), Some("v0.5.1"));
        assert!(r.phi * glicko::SCALE < 350.0, "the deviation shrank");
        assert!(r.sigma > 0.0);
    }
    // The two teams' results mirror each other.
    let (a, b) = (
        rating(&pool, season.id, t1).await,
        rating(&pool, season.id, t2).await,
    );
    assert_eq!((a.wins, a.draws, a.losses), (b.losses, b.draws, b.wins));
    assert!((a.mu > 0.0) == (a.wins > a.losses) || a.wins == a.losses);
    for sub in [s1, s2] {
        let (games, faults): (i32, i32) =
            sqlx::query_as("SELECT ranked_games, ranked_faults FROM submissions WHERE id = $1")
                .bind(sub)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((games, faults), (1, 0));
    }

    // The same job again (a retry after its game was recorded) adds nothing.
    sqlx::query("UPDATE jobs SET state = 'queued', attempts = 0")
        .execute(&pool)
        .await
        .unwrap();
    work(&app, settings(36_000)).await;
    assert_eq!(games(&pool).await.len(), 1);
    assert_eq!(rating(&pool, season.id, t1).await.games, 1);
}

#[sqlx::test]
async fn the_arena_and_the_sides_follow_the_seed(pool: PgPool) {
    if !have_runner("the_arena_and_the_sides_follow_the_seed") {
        return;
    }
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "S", 30).await;
    let t1 = db_team(&pool, "Starters").await;
    let s1 = db_ready_bot(
        &pool,
        Some(&app.store),
        t1,
        &std::fs::read(example("starter")).unwrap(),
    )
    .await;
    let t2 = db_team(&pool, "Chasers").await;
    let s2 = db_ready_bot(
        &pool,
        Some(&app.store),
        t2,
        &std::fs::read(example("chaser")).unwrap(),
    )
    .await;
    // The first seed of every arena on either side: eight jobs of one pair.
    let members = contest_pool();
    let mut seeds: Vec<i64> = Vec::new();
    for arena in &members {
        for side_a in 0..2u8 {
            let want = Draw {
                arena: Some(arena.clone()),
                side_a,
            };
            let seed = (1..10_000u64)
                .find(|s| draw(*s, &members) == want)
                .expect("a seed for every draw");
            seeds.push(seed as i64);
        }
    }
    for seed in &seeds {
        let payload = MatchPayload {
            season: season.id,
            round: 1,
            mode: "contest".to_owned(),
            team_a: t1,
            submission_a: s1,
            team_b: t2,
            submission_b: s2,
            seed: *seed,
        };
        sqlx::query("INSERT INTO jobs (id, kind, payload) VALUES ($1, 'match', $2)")
            .bind(Uuid::now_v7())
            .bind(serde_json::to_value(&payload).unwrap())
            .execute(&pool)
            .await
            .unwrap();
    }
    // A short cap: what is checked is where and as whom each game was played.
    work(&app, settings(300)).await;
    let rows = games(&pool).await;
    assert_eq!(rows.len(), seeds.len());
    if rows[0].result.get("arena").is_none() {
        eprintln!(
            "the_arena_and_the_sides_follow_the_seed: this kfm_arena names no arena; sides only"
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for row in &rows {
        assert!(seeds.contains(&row.seed));
        let want = drawn(row.seed, &row.result);
        assert_eq!(i16::from(want.side_a), row.side_a, "seed {}", row.seed);
        assert_eq!(
            row.result.get("arena").and_then(Value::as_str),
            want.arena.as_deref(),
            "seed {}",
            row.seed
        );
        // Side A's module sits on the team it drew.
        let on_a = row.result["teams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["team"] == row.side_a)
            .unwrap();
        assert_eq!(
            on_a["spec"].as_str().unwrap(),
            vec![format!("{s1}.wasm"); 5].join(",")
        );
        seen.insert((want.arena.clone(), want.side_a));
    }
    if rows[0].result.get("arena").is_some() {
        assert_eq!(seen.len(), 8, "every arena from either side: {seen:?}");
    }
    assert_eq!(rating(&pool, season.id, t1).await.games, 8);
}

#[sqlx::test]
async fn a_lost_game_is_recorded_again_the_same_by_the_retry(pool: PgPool) {
    if !have_runner("a_lost_game_is_recorded_again_the_same_by_the_retry") {
        return;
    }
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "S", 30).await;
    let t1 = db_team(&pool, "Starters").await;
    db_ready_bot(
        &pool,
        Some(&app.store),
        t1,
        &std::fs::read(example("starter")).unwrap(),
    )
    .await;
    let t2 = db_team(&pool, "Chasers").await;
    db_ready_bot(
        &pool,
        Some(&app.store),
        t2,
        &std::fs::read(example("chaser")).unwrap(),
    )
    .await;
    one_round(&pool).await;
    work(&app, settings(36_000)).await;
    let first = games(&pool).await;
    assert_eq!(first.len(), 1);
    // Forget the game's row, as if the worker died before recording it.
    sqlx::query("DELETE FROM matches")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET state = 'queued', attempts = 0")
        .execute(&pool)
        .await
        .unwrap();
    work(&app, settings(36_000)).await;
    let again = games(&pool).await;
    assert_eq!(again.len(), 1);
    assert_ne!(again[0].id, first[0].id);
    // Deterministic: the same draw and the same game.
    assert_eq!(again[0].side_a, first[0].side_a);
    assert_eq!(again[0].result, first[0].result);
    assert_eq!(again[0].winner_team, first[0].winner_team);
    // The game counted twice in the ratings here (the test deleted only the
    // row), which is what the uniqueness of (job, game) prevents in real runs.
    assert_eq!(rating(&pool, season.id, t1).await.games, 2);
}

#[sqlx::test]
async fn a_module_that_traps_in_a_ranked_match_is_recorded_not_rejected(pool: PgPool) {
    if !have_runner("a_module_that_traps_in_a_ranked_match_is_recorded_not_rejected") {
        return;
    }
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "S", 30).await;
    let trapping = hex::decode(TRAPPING).unwrap();
    assert!(kfm_ladder::wasm::check(&trapping).is_ok());
    // The trapping team rated far higher, so it is side A.
    let bad = db_team(&pool, "Trappers").await;
    let bad_sub = db_ready_bot(&pool, Some(&app.store), bad, &trapping).await;
    sqlx::query(
        "INSERT INTO ratings (season_id, team_id, mu, phi, sigma) VALUES ($1, $2, 2.0, 1.0, 0.06)",
    )
    .bind(season.id)
    .bind(bad)
    .execute(&pool)
    .await
    .unwrap();
    let good = db_team(&pool, "Starters").await;
    let good_sub = db_ready_bot(
        &pool,
        Some(&app.store),
        good,
        &std::fs::read(example("starter")).unwrap(),
    )
    .await;
    let payload = one_round(&pool).await;
    assert_eq!(payload.team_a, bad);
    work(&app, settings(3600)).await;
    let (state,): (String,) = sqlx::query_as("SELECT state FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "done");
    let rows = games(&pool).await;
    assert_eq!(rows.len(), 1);
    for row in &rows {
        let faults = row.faults_a.as_array().unwrap();
        assert_eq!(
            faults.len(),
            5,
            "every slot of the trapping side: {faults:?}"
        );
        let slots: Vec<u64> = faults.iter().map(|f| f["slot"].as_u64().unwrap()).collect();
        // Side A holds team side_a's slots: the even ones on team 0.
        assert!(
            slots.iter().all(|s| s % 2 == row.side_a as u64),
            "{slots:?}"
        );
        for fault in faults {
            assert!(fault["traps"].as_u64().unwrap() > 0, "{fault}");
            for key in ["slot", "traps", "over", "bad", "dead"] {
                assert!(fault.get(key).is_some(), "{key}");
            }
        }
        assert_eq!(row.faults_b, Value::Array(vec![]));
    }
    let counts = |sub: Uuid| {
        let pool = pool.clone();
        async move {
            let row: (String, i32, i32) = sqlx::query_as(
                "SELECT state, ranked_games, ranked_faults FROM submissions WHERE id = $1",
            )
            .bind(sub)
            .fetch_one(&pool)
            .await
            .unwrap();
            row
        }
    };
    assert_eq!(counts(bad_sub).await, ("ready".to_owned(), 1, 1));
    assert_eq!(counts(good_sub).await, ("ready".to_owned(), 1, 0));
}

#[sqlx::test]
async fn an_engine_release_widens_the_deviation_before_the_next_game(pool: PgPool) {
    if !have_runner("an_engine_release_widens_the_deviation_before_the_next_game") {
        return;
    }
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "S", 30).await;
    let t1 = db_team(&pool, "Old hands").await;
    db_ready_bot(
        &pool,
        Some(&app.store),
        t1,
        &std::fs::read(example("starter")).unwrap(),
    )
    .await;
    let t2 = db_team(&pool, "Newcomers").await;
    db_ready_bot(
        &pool,
        Some(&app.store),
        t2,
        &std::fs::read(example("chaser")).unwrap(),
    )
    .await;
    // t1 is settled (RD 50) from games under an older release.
    let settled = Rating::from_glicko(1600.0, 50.0, 0.06);
    sqlx::query(
        "INSERT INTO ratings (season_id, team_id, mu, phi, sigma, games, wins, engine_tag) \
         VALUES ($1, $2, $3, $4, $5, 40, 30, 'v0.4.0')",
    )
    .bind(season.id)
    .bind(t1)
    .bind(settled.mu)
    .bind(settled.phi)
    .bind(settled.sigma)
    .execute(&pool)
    .await
    .unwrap();
    one_round(&pool).await;
    work(&app, settings(36_000)).await;
    let rows = games(&pool).await;
    // Replay the game through Glicko-2: t1 widened once, before it.
    let mut a = glicko::widen(settled);
    let mut b = Rating::default();
    for row in &rows {
        assert_eq!(row.team_a, t1);
        let score_a = match row.winner_team {
            None => 0.5,
            Some(w) if w == t1 => 1.0,
            Some(_) => 0.0,
        };
        let (na, nb) = glicko::rate_pair(a, b, score_a);
        assert!((row.delta_a - (na.rating() - a.rating())).abs() < 1e-6);
        assert!((row.delta_b - (nb.rating() - b.rating())).abs() < 1e-6);
        a = na;
        b = nb;
    }
    let r1 = rating(&pool, season.id, t1).await;
    assert!(
        (r1.mu - a.mu).abs() < 1e-9 && (r1.phi - a.phi).abs() < 1e-9,
        "{r1:?} {a:?}"
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(r1.games, 41);
    assert_eq!(r1.engine_tag.as_deref(), Some("v0.5.1"));
    // Without the widening the update would have been smaller.
    let unwidened = glicko::rate_pair(settled, Rating::default(), 1.0).0;
    let widened = glicko::rate_pair(glicko::widen(settled), Rating::default(), 1.0).0;
    assert!(widened.rating() - 1600.0 > unwidened.rating() - 1600.0);
    let r2 = rating(&pool, season.id, t2).await;
    assert!((r2.mu - b.mu).abs() < 1e-9);
}

#[sqlx::test]
async fn a_failing_runner_fails_the_job_after_its_attempts(pool: PgPool) {
    if !have_runner("a_failing_runner_fails_the_job_after_its_attempts") {
        return;
    }
    let app = start(pool.clone()).await;
    db_season(&pool, "S", 30).await;
    for (name, module) in [("Starters", "starter"), ("Chasers", "chaser")] {
        let team = db_team(&pool, name).await;
        db_ready_bot(
            &pool,
            Some(&app.store),
            team,
            &std::fs::read(example(module)).unwrap(),
        )
        .await;
    }
    one_round(&pool).await;
    work(
        &app,
        MatchSettings {
            arena: PathBuf::from("/bin/false"),
            ..settings(600)
        },
    )
    .await;
    let (state, attempts, error): (String, i32, Option<String>) =
        sqlx::query_as("SELECT state, attempts, error FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), attempts), ("failed", 3));
    assert!(error.unwrap().starts_with("kfm_arena exited Some(1)"));
    assert!(games(&pool).await.is_empty());
    let (ratings,): (i64,) = sqlx::query_as("SELECT count(*) FROM ratings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ratings, 0);
}

#[sqlx::test]
async fn a_job_whose_season_ended_records_nothing(pool: PgPool) {
    if !have_store("a_job_whose_season_ended_records_nothing") {
        return;
    }
    let app = start(pool.clone()).await;
    let now = time::OffsetDateTime::now_utc();
    let ended = kfm_ladder::seasons::create(
        &pool,
        "ended",
        now - time::Duration::days(9),
        now - time::Duration::days(1),
        "contest",
    )
    .await
    .unwrap();
    let t1 = db_team(&pool, "Alpha").await;
    let s1 = db_ready_bot(&pool, Some(&app.store), t1, b"a").await;
    let t2 = db_team(&pool, "Beta").await;
    let s2 = db_ready_bot(&pool, Some(&app.store), t2, b"b").await;
    let payload = MatchPayload {
        season: ended.id,
        round: 1,
        mode: "contest".to_owned(),
        team_a: t1,
        submission_a: s1,
        team_b: t2,
        submission_b: s2,
        seed: 7,
    };
    sqlx::query("INSERT INTO jobs (id, kind, payload) VALUES ($1, 'match', $2)")
        .bind(Uuid::now_v7())
        .bind(serde_json::to_value(&payload).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    // The runner is never started (a missing binary would fail the job).
    work(
        &app,
        MatchSettings {
            arena: PathBuf::from("/nonexistent/kfm_arena"),
            ..settings(600)
        },
    )
    .await;
    let (state,): (String,) = sqlx::query_as("SELECT state FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "done");
    assert!(games(&pool).await.is_empty());
}
