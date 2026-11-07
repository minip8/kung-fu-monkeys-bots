//! The worker loop (`kfm-ladder worker build|match`): claim a job of the
//! role's kinds, run it to the end while heartbeating, record the outcome,
//! and now and then sweep the jobs whose worker died. A shutdown request
//! (SIGTERM, Ctrl-C) stops claiming; the job in hand finishes first.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;

use crate::jobs::{self, Job, Kind, Outcome, Swept};

/// How a job ended, when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    /// Something outside the job failed (the store, the database, docker,
    /// kfm_arena's own I/O): try again later, up to the attempt cap.
    Retry(String),
    /// The job cannot succeed (a payload naming nothing, a kind not served).
    Fail(String),
}

impl JobError {
    pub fn retry(detail: impl std::fmt::Display) -> JobError {
        JobError::Retry(detail.to_string())
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Retry(e) | JobError::Fail(e) => f.write_str(e),
        }
    }
}

impl From<sqlx::Error> for JobError {
    fn from(err: sqlx::Error) -> Self {
        JobError::Retry(format!("database: {err}"))
    }
}

/// What a role does with the jobs it claims.
pub trait Handler: Send + Sync {
    /// The kinds this role claims.
    fn kinds(&self) -> &[Kind];

    /// Runs one job. `Ok` means the job did its work, whatever the
    /// submission's fate (a failed build is a done job).
    fn run(&self, job: &Job) -> impl Future<Output = Result<(), JobError>> + Send;

    /// Called once a job failed for good (its attempts used up, a permanent
    /// error, or swept after its last attempt), so the role can settle what
    /// the job was about (a submission left `building`).
    fn gave_up(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        error: &str,
    ) -> impl Future<Output = ()> + Send;
}

/// The loop's timings. The defaults are the service's; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// How long to wait before looking again when no job is queued.
    pub poll: Duration,
    /// How often a running job's heartbeat is written.
    pub heartbeat: Duration,
    /// A running job whose heartbeat is older than this is swept.
    pub stale: Duration,
    /// How often this worker sweeps.
    pub sweep_every: Duration,
    /// How long a job that failed with [`JobError::Retry`] waits.
    pub retry_delay: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_secs(2),
            heartbeat: Duration::from_secs(15),
            stale: Duration::from_secs(5 * 60),
            sweep_every: Duration::from_secs(60),
            retry_delay: Duration::from_secs(30),
        }
    }
}

/// A worker's name in `jobs.claimed_by`: the host, the process and 32
/// random bits (two workers on one host never share a name).
pub fn worker_name(role: &str) -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".to_owned());
    let tag = hex::encode(crate::crypto::random::<4>());
    format!("{role}@{host}:{}:{tag}", std::process::id())
}

async fn settle_swept<H: Handler>(handler: &H, swept: &[Swept]) {
    for job in swept {
        tracing::warn!(job = %job.id, kind = %job.kind, state = %job.state,
            worker = job.claimed_by.as_deref().unwrap_or("?"), "swept a stale job");
        if job.state == "failed" {
            handler
                .gave_up(
                    &job.kind,
                    &job.payload,
                    "the job's worker stopped, attempts used up",
                )
                .await;
        }
    }
}

/// Runs one claimed job to its end and records the outcome.
pub async fn run_job<H: Handler>(
    db: &PgPool,
    handler: &H,
    worker: &str,
    job: Job,
    timing: &Timing,
) {
    tracing::info!(job = %job.id, kind = %job.kind, attempt = job.attempts, "job claimed");
    let started = std::time::Instant::now();
    let result = {
        let work = handler.run(&job);
        tokio::pin!(work);
        let mut beat = tokio::time::interval(timing.heartbeat);
        beat.tick().await;
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = beat.tick() => match jobs::heartbeat(db, job.id, worker).await {
                    Ok(true) => {}
                    Ok(false) => tracing::warn!(job = %job.id, "the claim was lost; finishing anyway"),
                    Err(err) => tracing::warn!(job = %job.id, error = %err, "heartbeat failed"),
                },
            }
        }
    };
    let secs = started.elapsed().as_secs_f64();
    let recorded = match &result {
        Ok(()) => jobs::finish(db, job.id, worker).await.map(|held| {
            if held {
                tracing::info!(job = %job.id, kind = %job.kind, secs, "job done");
            } else {
                tracing::warn!(job = %job.id, "job done after its claim was lost");
            }
        }),
        Err(err) => {
            let retry = matches!(err, JobError::Retry(_));
            let message = err.to_string();
            match jobs::fail(db, job.id, worker, &message, retry, timing.retry_delay).await {
                Ok(Outcome::Requeued) => {
                    tracing::warn!(job = %job.id, kind = %job.kind, error = %message, "job failed; re-queued");
                    Ok(())
                }
                Ok(Outcome::Failed) => {
                    tracing::error!(job = %job.id, kind = %job.kind, error = %message, "job failed");
                    handler.gave_up(&job.kind, &job.payload, &message).await;
                    Ok(())
                }
                Ok(Outcome::Lost) => {
                    tracing::warn!(job = %job.id, error = %message, "job failed after its claim was lost");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(err) = recorded {
        // The heartbeat goes stale and the sweeper re-queues it.
        tracing::error!(job = %job.id, error = %err, "could not record the job's outcome");
    }
}

/// The loop: until `shutdown` turns true, claim and run jobs of the
/// handler's kinds, sweeping every `timing.sweep_every`.
pub async fn run<H: Handler>(
    db: PgPool,
    handler: H,
    worker: String,
    timing: Timing,
    mut shutdown: watch::Receiver<bool>,
) {
    let kinds: Vec<&str> = handler.kinds().iter().map(|k| k.as_str()).collect();
    tracing::info!(%worker, ?kinds, "worker started");
    let mut next_sweep = tokio::time::Instant::now();
    let mut db_failing = false;
    while !*shutdown.borrow() {
        if tokio::time::Instant::now() >= next_sweep {
            next_sweep = tokio::time::Instant::now() + timing.sweep_every;
            match jobs::sweep(&db, timing.stale).await {
                Ok(swept) => settle_swept(&handler, &swept).await,
                Err(err) => tracing::warn!(error = %err, "sweep failed"),
            }
        }
        match jobs::claim(&db, &worker, handler.kinds()).await {
            Ok(Some(job)) => {
                db_failing = false;
                run_job(&db, &handler, &worker, job, &timing).await;
                continue;
            }
            Ok(None) => db_failing = false,
            Err(err) => {
                if !db_failing {
                    tracing::warn!(error = %err, "claim failed; retrying");
                }
                db_failing = true;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(timing.poll) => {}
            _ = shutdown.changed() => {}
        }
    }
    tracing::info!(%worker, "worker stopped");
}

/// A watch that turns true on SIGTERM or Ctrl-C.
pub fn shutdown_on_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let term = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();
        tokio::select! {
            () = ctrl_c => {}
            () = term => {}
        }
        tracing::info!("stopping: the job in hand finishes first");
        let _ = tx.send(true);
    });
    rx
}
