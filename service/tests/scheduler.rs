//! The scheduler over Postgres: rounds only inside a season, the eligible
//! teams, the pairs as match jobs with their seeds, the round interval, busy
//! pairs, and the advisory lock that keeps a second instance idle.

mod common;

use std::collections::HashSet;
use std::time::Duration;

use common::{db_ready_bot, db_season, db_team};
use kfm_ladder::scheduler::{self, Entrant, MatchPayload, RoundOutcome, SchedulerSettings};
use kfm_ladder::seasons;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;
use uuid::Uuid;

const HOUR: Duration = Duration::from_secs(3600);

async fn round(pool: &PgPool, every: Duration, k: usize) -> RoundOutcome {
    let mut conn = pool.acquire().await.unwrap();
    scheduler::run_round(&mut conn, every, k).await.unwrap()
}

/// `n` teams with ready active bots; the i-th rated 1500 + 100·(n - i) in
/// `season` when given. (team, submission) per team, best first.
async fn teams(pool: &PgPool, n: usize, rated_in: Option<Uuid>) -> Vec<(Uuid, Uuid)> {
    let mut out = Vec::new();
    for i in 0..n {
        let team = db_team(pool, &format!("Team {i}")).await;
        let sub = db_ready_bot(pool, None, team, format!("module {i}").as_bytes()).await;
        if let Some(season) = rated_in {
            let r = 1500.0 + 100.0 * (n - i) as f64;
            sqlx::query(
                "INSERT INTO ratings (season_id, team_id, mu, phi, sigma) VALUES ($1, $2, $3, 1.0, 0.06)",
            )
            .bind(season)
            .bind(team)
            .bind((r - 1500.0) / kfm_ladder::glicko::SCALE)
            .execute(pool)
            .await
            .unwrap();
        }
        out.push((team, sub));
    }
    out
}

async fn payloads(pool: &PgPool) -> Vec<(MatchPayload, String)> {
    let rows: Vec<(serde_json::Value, String)> = sqlx::query_as(
        "SELECT payload, state FROM jobs WHERE kind = 'match' ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(p, s)| (serde_json::from_value(p).unwrap(), s))
        .collect()
}

#[sqlx::test]
async fn nothing_is_scheduled_outside_a_season(pool: PgPool) {
    teams(&pool, 4, None).await;
    assert_eq!(round(&pool, HOUR, 3).await, RoundOutcome::NoSeason);
    let now = OffsetDateTime::now_utc();
    seasons::create(
        &pool,
        "over",
        now - time::Duration::days(9),
        now - time::Duration::days(2),
        "contest",
    )
    .await
    .unwrap();
    seasons::create(
        &pool,
        "soon",
        now + time::Duration::days(2),
        now + time::Duration::days(9),
        "contest",
    )
    .await
    .unwrap();
    assert_eq!(round(&pool, HOUR, 3).await, RoundOutcome::NoSeason);
    let (jobs, rounds): (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM rounds)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((jobs, rounds), (0, 0));
}

#[sqlx::test]
async fn fewer_than_two_ready_active_teams_make_no_round(pool: PgPool) {
    let season = db_season(&pool, "S", 30).await;
    // A team with no submission, and one whose only submission is not ready.
    db_team(&pool, "Nobody").await;
    let pending = db_team(&pool, "Pending").await;
    sqlx::query(
        "INSERT INTO submissions (id, team_id, manifest, state) VALUES ($1, $2, 'language = \"c\"', 'building')",
    )
    .bind(Uuid::now_v7())
    .bind(pending)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        round(&pool, HOUR, 3).await,
        RoundOutcome::TooFew {
            season: season.id,
            teams: 0
        }
    );
    teams(&pool, 1, None).await;
    assert_eq!(
        round(&pool, HOUR, 3).await,
        RoundOutcome::TooFew {
            season: season.id,
            teams: 1
        }
    );
    let (rounds,): (i64,) = sqlx::query_as("SELECT count(*) FROM rounds")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rounds, 0);
}

#[sqlx::test]
async fn a_round_queues_one_match_job_per_pair_with_its_seed(pool: PgPool) {
    let season = db_season(&pool, "S", 30).await;
    let list = teams(&pool, 5, Some(season.id)).await;
    let RoundOutcome::Ran {
        season: s,
        number,
        teams: n,
        pairs,
        busy,
    } = round(&pool, HOUR, 3).await
    else {
        panic!("no round")
    };
    assert_eq!((s, number, n, busy), (season.id, 1, 5, 0));
    // The same pairs as the pure function over the same ratings.
    let entrants: Vec<Entrant> = list
        .iter()
        .enumerate()
        .map(|(i, &(team, submission))| Entrant {
            team,
            submission,
            rating: 1500.0 + 100.0 * (5 - i) as f64,
        })
        .collect();
    let expected = scheduler::pair(&entrants, 3, &HashSet::new());
    let got: Vec<(Uuid, Uuid)> = pairs.iter().map(|p| (p.a.team, p.b.team)).collect();
    let want: Vec<(Uuid, Uuid)> = expected
        .iter()
        .map(|&(i, j)| (entrants[i].team, entrants[j].team))
        .collect();
    assert_eq!(got, want);
    assert!(pairs.len() <= 5 * 3 / 2);
    // The jobs carry the round, the mode and a seed from the four inputs.
    let jobs = payloads(&pool).await;
    assert_eq!(jobs.len(), pairs.len());
    for ((payload, state), p) in jobs.iter().zip(&pairs) {
        assert_eq!(state, "queued");
        assert_eq!(payload.season, season.id);
        assert_eq!(payload.round, 1);
        assert_eq!(payload.mode, "contest");
        assert_eq!((payload.team_a, payload.team_b), (p.a.team, p.b.team));
        assert_eq!(
            (payload.submission_a, payload.submission_b),
            (p.a.submission, p.b.submission)
        );
        assert_eq!(
            payload.seed,
            scheduler::seed(season.id, 1, p.a.submission, p.b.submission)
        );
        assert!(payload.seed >= 0);
    }
    let (teams_n, pairs_n): (i32, i32) =
        sqlx::query_as("SELECT teams, pairs FROM rounds WHERE season_id = $1 AND number = 1")
            .bind(season.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((teams_n, pairs_n as usize), (5, pairs.len()));
}

#[sqlx::test]
async fn a_round_waits_for_its_interval_and_skips_busy_pairs(pool: PgPool) {
    let season = db_season(&pool, "S", 30).await;
    teams(&pool, 4, Some(season.id)).await;
    let RoundOutcome::Ran { pairs: first, .. } = round(&pool, HOUR, 2).await else {
        panic!("no round")
    };
    assert_eq!(first.len(), 4);
    // Not due within the hour.
    assert_eq!(round(&pool, HOUR, 2).await, RoundOutcome::NotDue);
    // Due at once with a zero interval, but every pair is still queued.
    let RoundOutcome::Ran {
        number,
        pairs,
        busy,
        ..
    } = round(&pool, Duration::ZERO, 2).await
    else {
        panic!("no round")
    };
    assert_eq!((number, pairs.len(), busy), (2, 0, 4));
    // Two of the four finish: only those two pairs are queued again.
    sqlx::query(
        "UPDATE jobs SET state = 'done' WHERE id IN (SELECT id FROM jobs ORDER BY created_at, id LIMIT 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let RoundOutcome::Ran {
        number,
        pairs,
        busy,
        ..
    } = round(&pool, Duration::ZERO, 2).await
    else {
        panic!("no round")
    };
    assert_eq!((number, busy), (3, 2));
    let again: HashSet<(Uuid, Uuid)> = pairs.iter().map(|p| (p.a.team, p.b.team)).collect();
    let done: HashSet<(Uuid, Uuid)> = first[..2].iter().map(|p| (p.a.team, p.b.team)).collect();
    assert_eq!(again, done);
    // A new round has new seeds for the same pairs.
    let jobs = payloads(&pool).await;
    assert_eq!(jobs.len(), 6);
    assert_ne!(jobs[0].0.seed, jobs[4].0.seed);
    assert_eq!(jobs[4].0.round, 3);
}

#[sqlx::test]
async fn a_team_that_changes_its_active_submission_plays_the_new_one(pool: PgPool) {
    let season = db_season(&pool, "S", 30).await;
    let list = teams(&pool, 2, None).await;
    round(&pool, HOUR, 1).await;
    sqlx::query("UPDATE jobs SET state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let newer = db_ready_bot(&pool, None, list[0].0, b"v2").await;
    let RoundOutcome::Ran { pairs, .. } = round(&pool, Duration::ZERO, 1).await else {
        panic!("no round")
    };
    let p = &pairs[0];
    let subs = [p.a.submission, p.b.submission];
    assert!(subs.contains(&newer) && subs.contains(&list[1].1), "{p:?}");
    let _ = season;
}

#[sqlx::test]
async fn the_loop_schedules_only_while_it_holds_the_lock(pool: PgPool) {
    db_season(&pool, "S", 30).await;
    teams(&pool, 2, None).await;
    // Another instance holds the lock.
    let mut other = pool.acquire().await.unwrap().detach();
    assert!(scheduler::try_lock(&mut other).await.unwrap());
    let (stop, stopped) = watch::channel(false);
    let settings = SchedulerSettings::new(Duration::ZERO, 3);
    let task = tokio::spawn(scheduler::run(pool.clone(), settings, stopped));
    let rounds = || async {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM rounds")
            .fetch_one(&pool)
            .await
            .unwrap();
        n
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(rounds().await, 0, "an idle instance made a round");
    // The holder goes: this instance takes over.
    assert!(scheduler::unlock(&mut other).await.unwrap());
    let mut waited = 0;
    while rounds().await == 0 {
        assert!(waited < 100, "the loop never took the lock");
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    // And now holds it: the other connection cannot take it.
    assert!(!scheduler::try_lock(&mut other).await.unwrap());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    // Stopping released it.
    assert!(scheduler::try_lock(&mut other).await.unwrap());
}
