//! The job queue: the `jobs` table (architecture v0.63 §11: "the jobs table
//! is the queue"; no broker).

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// What a job does; the worker roles claim by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A submission's source into a module (`{"submission": id}`).
    Build,
    /// A built module through the checks and the smoke match (`{"submission": id}`).
    Validate,
    /// One two-sided `kfm_arena` run (step 4).
    Match,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Build, Kind::Validate, Kind::Match];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Build => "build",
            Kind::Validate => "validate",
            Kind::Match => "match",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// Queues a job inside `tx` (committed with the row it is about); returns its id.
pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    kind: Kind,
    payload: Value,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO jobs (id, kind, payload) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(kind.as_str())
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}

/// A claimed job.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    /// This claim's attempt, 1-based.
    pub attempts: i32,
    pub max_attempts: i32,
}

impl Job {
    /// The `submission` id of a build or validate payload.
    pub fn submission(&self) -> Option<Uuid> {
        self.payload
            .get("submission")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    }
}

/// Claims the oldest runnable queued job of one of `kinds` for `worker`, or
/// `None`. Concurrent claimers skip each other's locked rows, so a job is
/// claimed once.
pub async fn claim(db: &PgPool, worker: &str, kinds: &[Kind]) -> Result<Option<Job>, sqlx::Error> {
    let kinds: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    sqlx::query_as(
        "UPDATE jobs SET state = 'running', claimed_by = $1, claimed_at = now(), \
         heartbeat_at = now(), attempts = attempts + 1, error = NULL \
         WHERE id = (SELECT id FROM jobs WHERE state = 'queued' AND kind = ANY($2) \
                     AND run_after <= now() ORDER BY created_at, id LIMIT 1 \
                     FOR UPDATE SKIP LOCKED) \
         RETURNING id, kind, payload, attempts, max_attempts",
    )
    .bind(worker)
    .bind(&kinds)
    .fetch_optional(db)
    .await
}

/// Records that `worker` still runs `job`; `false` when the claim was lost
/// (the sweeper re-queued it), and the worker's result will not count.
pub async fn heartbeat(db: &PgPool, job: Uuid, worker: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE jobs SET heartbeat_at = now() \
         WHERE id = $1 AND claimed_by = $2 AND state = 'running'",
    )
    .bind(job)
    .bind(worker)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Marks `job` done; `false` when `worker` no longer holds it.
pub async fn finish(db: &PgPool, job: Uuid, worker: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE jobs SET state = 'done', finished_at = now(), error = NULL \
         WHERE id = $1 AND claimed_by = $2 AND state = 'running'",
    )
    .bind(job)
    .bind(worker)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// What became of a job that did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Queued again, to run after the delay.
    Requeued,
    /// Failed for good: attempts used up, or the failure is permanent.
    Failed,
    /// The worker no longer held the job; nothing was written.
    Lost,
}

/// A failure of `job`: re-queued after `delay` while attempts remain and
/// `retry` is set, else failed with `error`.
pub async fn fail(
    db: &PgPool,
    job: Uuid,
    worker: &str,
    error: &str,
    retry: bool,
    delay: std::time::Duration,
) -> Result<Outcome, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "UPDATE jobs SET \
           state = CASE WHEN $4 AND attempts < max_attempts THEN 'queued' ELSE 'failed' END, \
           run_after = now() + make_interval(secs => $5), \
           claimed_by = NULL, \
           finished_at = CASE WHEN $4 AND attempts < max_attempts THEN NULL ELSE now() END, \
           error = $3 \
         WHERE id = $1 AND claimed_by = $2 AND state = 'running' RETURNING state",
    )
    .bind(job)
    .bind(worker)
    .bind(error)
    .bind(retry)
    .bind(delay.as_secs_f64())
    .fetch_optional(db)
    .await?;
    Ok(match row.as_ref().map(|(s,)| s.as_str()) {
        Some("queued") => Outcome::Requeued,
        Some(_) => Outcome::Failed,
        None => Outcome::Lost,
    })
}

/// A job the sweeper found stale.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Swept {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    /// `queued` (re-queued) or `failed` (attempts used up).
    pub state: String,
    pub claimed_by: Option<String>,
}

/// Re-queues every running job whose heartbeat is older than `stale` (its
/// worker died or lost the database), or fails it once its attempts are used.
pub async fn sweep(db: &PgPool, stale: std::time::Duration) -> Result<Vec<Swept>, sqlx::Error> {
    sqlx::query_as(
        "UPDATE jobs j SET \
           state = CASE WHEN j.attempts < j.max_attempts THEN 'queued' ELSE 'failed' END, \
           finished_at = CASE WHEN j.attempts < j.max_attempts THEN NULL ELSE now() END, \
           error = 'the worker ' || coalesce(j.claimed_by, '?') || ' stopped heartbeating', \
           claimed_by = NULL \
         FROM (SELECT id, claimed_by FROM jobs WHERE state = 'running' \
               AND heartbeat_at < now() - make_interval(secs => $1) FOR UPDATE SKIP LOCKED) old \
         WHERE j.id = old.id \
         RETURNING j.id, j.kind, j.payload, j.state, old.claimed_by",
    )
    .bind(stale.as_secs_f64())
    .fetch_all(db)
    .await
}
