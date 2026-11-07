//! The job queue over Postgres: the claim (by kind, oldest first, never twice
//! under concurrent workers), heartbeats and lost claims, retries up to the
//! attempt cap, the sweeper, the worker loop with its graceful stop, and the
//! match role's stub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kfm_ladder::jobs::{self, Job, Kind, Outcome};
use kfm_ladder::worker::{self, Handler, JobError, Timing};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

async fn enqueue(db: &PgPool, kind: Kind, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 0..n {
        let mut tx = db.begin().await.unwrap();
        ids.push(
            jobs::enqueue(&mut tx, kind, json!({ "n": i }))
                .await
                .unwrap(),
        );
        tx.commit().await.unwrap();
    }
    ids
}

async fn job_state(db: &PgPool, id: Uuid) -> (String, i32, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, attempts, claimed_by, error FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn age_heartbeat(db: &PgPool, id: Uuid, minutes: i32) {
    sqlx::query("UPDATE jobs SET heartbeat_at = now() - make_interval(mins => $2) WHERE id = $1")
        .bind(id)
        .bind(minutes)
        .execute(db)
        .await
        .unwrap();
}

#[sqlx::test]
async fn claims_take_the_oldest_job_of_the_kinds_asked(pool: PgPool) {
    let matches = enqueue(&pool, Kind::Match, 1).await;
    let builds = enqueue(&pool, Kind::Build, 2).await;
    let first = jobs::claim(&pool, "w1", &[Kind::Build, Kind::Validate])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, builds[0]);
    assert_eq!(first.kind, "build");
    assert_eq!(first.attempts, 1);
    assert_eq!(first.payload, json!({ "n": 0 }));
    let second = jobs::claim(&pool, "w1", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, builds[1]);
    assert!(jobs::claim(&pool, "w1", &[Kind::Build, Kind::Validate])
        .await
        .unwrap()
        .is_none());
    let m = jobs::claim(&pool, "w2", &[Kind::Match])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.id, matches[0]);
    let (state, attempts, by, _) = job_state(&pool, first.id).await;
    assert_eq!(
        (state.as_str(), attempts, by.as_deref()),
        ("running", 1, Some("w1"))
    );

    // A job waiting for its run_after is not claimed yet.
    let later = enqueue(&pool, Kind::Validate, 1).await[0];
    sqlx::query("UPDATE jobs SET run_after = now() + interval '1 hour' WHERE id = $1")
        .bind(later)
        .execute(&pool)
        .await
        .unwrap();
    assert!(jobs::claim(&pool, "w1", &[Kind::Validate])
        .await
        .unwrap()
        .is_none());
}

#[sqlx::test]
async fn two_workers_never_claim_the_same_job(pool: PgPool) {
    let ids = enqueue(&pool, Kind::Build, 40).await;
    let claimer = |name: &'static str| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(job) = jobs::claim(&pool, name, &[Kind::Build]).await.unwrap() {
                got.push(job.id);
                tokio::task::yield_now().await;
            }
            got
        })
    };
    let (a, b) = tokio::join!(claimer("alpha"), claimer("beta"));
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut all: Vec<Uuid> = a.iter().chain(b.iter()).copied().collect();
    all.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(all, expected, "every job claimed exactly once");
    // Both made progress (the claim does not serialise on one row lock).
    assert!(
        !a.is_empty() && !b.is_empty(),
        "alpha {} beta {}",
        a.len(),
        b.len()
    );
    let (running,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM jobs WHERE state = 'running' AND claimed_by IN ('alpha', 'beta')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(running, 40);
}

#[sqlx::test]
async fn retries_stop_at_the_attempt_cap(pool: PgPool) {
    let id = enqueue(&pool, Kind::Build, 1).await[0];
    for attempt in 1..=3 {
        let job = jobs::claim(&pool, "w", &[Kind::Build])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.attempts, attempt);
        let outcome = jobs::fail(&pool, id, "w", "store down", true, Duration::ZERO)
            .await
            .unwrap();
        let expected = if attempt < 3 {
            Outcome::Requeued
        } else {
            Outcome::Failed
        };
        assert_eq!(outcome, expected, "attempt {attempt}");
    }
    let (state, attempts, by, error) = job_state(&pool, id).await;
    assert_eq!(state, "failed");
    assert_eq!(attempts, 3);
    assert_eq!(by, None);
    assert_eq!(error.as_deref(), Some("store down"));
    // A permanent failure does not retry.
    let other = enqueue(&pool, Kind::Build, 1).await[0];
    jobs::claim(&pool, "w", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    let outcome = jobs::fail(
        &pool,
        other,
        "w",
        "no such submission",
        false,
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Failed);
    // A retry waits for its delay.
    let delayed = enqueue(&pool, Kind::Build, 1).await[0];
    jobs::claim(&pool, "w", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    jobs::fail(
        &pool,
        delayed,
        "w",
        "later",
        true,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    assert!(jobs::claim(&pool, "w", &[Kind::Build])
        .await
        .unwrap()
        .is_none());
}

#[sqlx::test]
async fn the_sweeper_requeues_stale_jobs_then_fails_them(pool: PgPool) {
    let id = enqueue(&pool, Kind::Build, 1).await[0];
    let fresh = enqueue(&pool, Kind::Validate, 1).await[0];
    jobs::claim(&pool, "dead", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    jobs::claim(&pool, "alive", &[Kind::Validate])
        .await
        .unwrap()
        .unwrap();
    age_heartbeat(&pool, id, 6).await;

    let swept = jobs::sweep(&pool, Duration::from_secs(300)).await.unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].id, id);
    assert_eq!(swept[0].state, "queued");
    assert_eq!(swept[0].claimed_by.as_deref(), Some("dead"));
    let (state, _, by, error) = job_state(&pool, id).await;
    assert_eq!((state.as_str(), by), ("queued", None));
    assert!(error.unwrap().contains("dead"));
    // The live job is untouched.
    assert_eq!(job_state(&pool, fresh).await.0, "running");

    // The dead worker's late writes do not count.
    assert!(!jobs::heartbeat(&pool, id, "dead").await.unwrap());
    assert!(!jobs::finish(&pool, id, "dead").await.unwrap());
    assert_eq!(
        jobs::fail(&pool, id, "dead", "x", true, Duration::ZERO)
            .await
            .unwrap(),
        Outcome::Lost
    );

    // Claimed again; a heartbeat keeps it; the third stale claim fails it.
    let again = jobs::claim(&pool, "next", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 2);
    assert!(jobs::heartbeat(&pool, id, "next").await.unwrap());
    assert!(jobs::sweep(&pool, Duration::from_secs(300))
        .await
        .unwrap()
        .is_empty());
    age_heartbeat(&pool, id, 6).await;
    jobs::sweep(&pool, Duration::from_secs(300)).await.unwrap();
    jobs::claim(&pool, "third", &[Kind::Build])
        .await
        .unwrap()
        .unwrap();
    age_heartbeat(&pool, id, 6).await;
    let swept = jobs::sweep(&pool, Duration::from_secs(300)).await.unwrap();
    assert_eq!(swept[0].state, "failed");
    assert_eq!(job_state(&pool, id).await.0, "failed");
}

/// A handler that records what it ran; a job whose payload says `slow` takes
/// a while, one that says `flaky` fails once for retry, `broken` for good.
#[derive(Clone, Default)]
struct Recorder {
    ran: Arc<Mutex<Vec<Value>>>,
    gave_up: Arc<Mutex<Vec<(String, String)>>>,
}

impl Handler for Recorder {
    fn kinds(&self) -> &[Kind] {
        &[Kind::Build, Kind::Validate]
    }

    async fn run(&self, job: &Job) -> Result<(), JobError> {
        self.ran.lock().unwrap().push(job.payload.clone());
        match job.payload["what"].as_str() {
            Some("slow") => tokio::time::sleep(Duration::from_millis(600)).await,
            Some("flaky") if job.attempts == 1 => return Err(JobError::Retry("flaky".to_owned())),
            Some("broken") => return Err(JobError::Fail("broken".to_owned())),
            _ => {}
        }
        Ok(())
    }

    async fn gave_up(&self, kind: &str, _payload: &Value, error: &str) {
        self.gave_up
            .lock()
            .unwrap()
            .push((kind.to_owned(), error.to_owned()));
    }
}

fn fast() -> Timing {
    Timing {
        poll: Duration::from_millis(20),
        heartbeat: Duration::from_millis(100),
        stale: Duration::from_secs(300),
        sweep_every: Duration::from_secs(60),
        retry_delay: Duration::ZERO,
    }
}

async fn add(pool: &PgPool, kind: Kind, payload: Value) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let id = jobs::enqueue(&mut tx, kind, payload).await.unwrap();
    tx.commit().await.unwrap();
    id
}

async fn wait_until(pool: &PgPool, sql: &str) {
    for _ in 0..200 {
        let (done,): (bool,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
        if done {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {sql}");
}

#[sqlx::test]
async fn the_loop_runs_records_retries_and_stops_after_the_job_in_hand(pool: PgPool) {
    let ok = add(&pool, Kind::Build, json!({ "what": "ok" })).await;
    let flaky = add(&pool, Kind::Validate, json!({ "what": "flaky" })).await;
    let broken = add(&pool, Kind::Build, json!({ "what": "broken" })).await;
    let ignored = add(&pool, Kind::Match, json!({})).await;
    let handler = Recorder::default();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(worker::run(
        pool.clone(),
        handler.clone(),
        "loop".to_owned(),
        fast(),
        rx,
    ));
    wait_until(
        &pool,
        "SELECT count(*) = 3 FROM jobs WHERE state IN ('done', 'failed') AND kind <> 'match'",
    )
    .await;
    assert_eq!(job_state(&pool, ok).await.0, "done");
    let (state, attempts, _, _) = job_state(&pool, flaky).await;
    assert_eq!((state.as_str(), attempts), ("done", 2));
    let (state, _, _, error) = job_state(&pool, broken).await;
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("failed", Some("broken"))
    );
    assert_eq!(
        handler.gave_up.lock().unwrap().clone(),
        [("build".to_owned(), "broken".to_owned())]
    );
    assert_eq!(job_state(&pool, ignored).await.0, "queued");

    // A slow job claimed before the stop finishes; nothing is claimed after.
    let slow = add(&pool, Kind::Build, json!({ "what": "slow" })).await;
    wait_until(
        &pool,
        &format!("SELECT state = 'running' FROM jobs WHERE id = '{slow}'"),
    )
    .await;
    let after = add(&pool, Kind::Build, json!({ "what": "after" })).await;
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job_state(&pool, slow).await.0, "done");
    assert_eq!(job_state(&pool, after).await.0, "queued");
}

#[sqlx::test]
async fn the_loop_settles_what_the_sweeper_gives_up(pool: PgPool) {
    let id = add(&pool, Kind::Build, json!({ "what": "lost" })).await;
    sqlx::query(
        "UPDATE jobs SET state = 'running', attempts = 3, claimed_by = 'gone', \
         heartbeat_at = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let handler = Recorder::default();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(worker::run(
        pool.clone(),
        handler.clone(),
        "sweeper".to_owned(),
        fast(),
        rx,
    ));
    wait_until(
        &pool,
        &format!("SELECT state = 'failed' FROM jobs WHERE id = '{id}'"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(true).unwrap();
    task.await.unwrap();
    let gave_up = handler.gave_up.lock().unwrap().clone();
    assert_eq!(gave_up.len(), 1);
    assert_eq!(gave_up[0].0, "build");
    assert!(handler.ran.lock().unwrap().is_empty());
}

#[sqlx::test]
async fn the_match_role_fails_a_job_that_names_no_match_at_once(pool: PgPool) {
    let id = add(&pool, Kind::Match, json!({})).await;
    let (tx, rx) = tokio::sync::watch::channel(false);
    // Nothing is fetched or run for a payload that is not a match.
    let store = kfm_ladder::Store::new(&kfm_ladder::config::S3Config {
        endpoint: "http://127.0.0.1:9".parse().unwrap(),
        bucket: "kfm".to_owned(),
        access_key: "a".to_owned(),
        secret_key: "s".to_owned(),
        region: "auto".to_owned(),
    })
    .unwrap();
    let handler = kfm_ladder::match_worker::MatchWorker::new(
        pool.clone(),
        store,
        kfm_ladder::match_worker::MatchSettings {
            arena: "/nonexistent/kfm_arena".into(),
            engine_tag: "v0.5.1".to_owned(),
            ticks: 600,
            work_dir: std::env::temp_dir(),
            timeout: Duration::from_secs(5),
        },
    );
    let task = tokio::spawn(worker::run(
        pool.clone(),
        handler,
        "match".to_owned(),
        fast(),
        rx,
    ));
    wait_until(
        &pool,
        &format!("SELECT state = 'failed' FROM jobs WHERE id = '{id}'"),
    )
    .await;
    tx.send(true).unwrap();
    task.await.unwrap();
    let (_, attempts, _, error) = job_state(&pool, id).await;
    assert_eq!(attempts, 1, "a permanent failure is not retried");
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("the payload is not a match")),
        "{error:?}"
    );
}
