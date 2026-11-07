//! The build worker's jobs (`kfm-ladder worker build`; architecture §8 "The
//! build sandbox", §11 "M7 step 3 design").
//!
//! `build`: the submission's zip from the store, its entry directory unpacked
//! (checked again: the archive is hostile and this process is outside the
//! sandbox) into a fresh `SRC` under `KFM_LADDER_BUILD_DIR`, then one
//! throwaway sibling container of `KFM_BUILD_IMAGE` through the host's Docker
//! socket with the §8 flags (plus `--name`, so a build that overruns the
//! worker's own clock can be removed):
//!
//! ```text
//! docker run --rm --name kfm-build-<job> --network none --memory 2g --cpus 1 --pids-limit 256
//!   --read-only --tmpfs /tmp --user builder -v SRC:/src:ro -v OUT:/out IMAGE timeout 120 kfm-build
//! ```
//!
//! Exit 0 and `OUT/bot.wasm` store the module (`modules/<s>.wasm`, its
//! SHA-256 on the row) and queue a `validate` job; anything else fails the
//! submission. `validate`: the module through [`crate::wasm::check`] and the
//! smoke match ([`crate::arena`]) with the released `kfm_arena`; a fault
//! rejects it, a clean run makes it `ready`. The log (`logs/<s>.txt`) is
//! always stored, the build's part first and validation's appended.
//!
//! A failure outside the submission (the store, Docker's daemon, the
//! runner) is a [`JobError::Retry`]: the job runs again, and after its last
//! attempt the submission fails with that reason.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::config::{ConfigError, WorkerConfig};
use crate::jobs::{self, Job, Kind};
use crate::store::{self, Store};
use crate::worker::{Handler, JobError};
use crate::{archive, arena, crypto, manifest, retention, wasm};

/// The sandbox's own limit (`timeout 120` inside the container).
pub const SANDBOX_SECONDS: u64 = 120;
/// The worker's limit on one `docker run`: the sandbox's plus room for the
/// container's start and teardown (slower under gVisor).
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(SANDBOX_SECONDS + 60);
/// The most log a build or a validation keeps, each.
pub const MAX_LOG_BYTES: usize = 1024 * 1024;
/// The mode the smoke match plays (the season's ruleset, architecture §8).
pub const SMOKE_MODE: &str = "contest";

/// What the build worker needs beyond the shared [`WorkerConfig`].
#[derive(Debug, Clone)]
pub struct BuildSettings {
    /// `KFM_BUILD_IMAGE`.
    pub image: String,
    /// `KFM_LADDER_BUILD_DIR`: the same path on the host and in the worker's
    /// container (the daemon resolves the sandbox's `-v` paths on the host).
    pub build_dir: PathBuf,
    /// `KFM_LADDER_ARENA`.
    pub arena: PathBuf,
    pub engine_tag: String,
    /// The Docker CLI (`docker`; tests may point elsewhere).
    pub docker: PathBuf,
    pub build_timeout: Duration,
    pub smoke_timeout: Duration,
}

impl BuildSettings {
    pub fn from_config(config: &WorkerConfig) -> Result<BuildSettings, ConfigError> {
        let missing = |variable: &'static str| ConfigError {
            variable,
            problem: "is required for worker build and not set".to_owned(),
        };
        let image = config
            .build_image
            .clone()
            .ok_or_else(|| missing("KFM_BUILD_IMAGE"))?;
        let build_dir = config
            .build_dir
            .clone()
            .ok_or_else(|| missing("KFM_LADDER_BUILD_DIR"))?;
        if !build_dir.is_absolute() {
            return Err(ConfigError {
                variable: "KFM_LADDER_BUILD_DIR",
                problem: "must be an absolute path (the host resolves it)".to_owned(),
            });
        }
        let arena = config
            .arena
            .clone()
            .ok_or_else(|| missing("KFM_LADDER_ARENA"))?;
        Ok(BuildSettings {
            image,
            build_dir,
            arena,
            engine_tag: config.engine_tag.clone(),
            docker: PathBuf::from("docker"),
            build_timeout: BUILD_TIMEOUT,
            smoke_timeout: arena::SMOKE_TIMEOUT,
        })
    }
}

/// A log of what a job did, capped.
#[derive(Default)]
struct Log {
    text: String,
    truncated: bool,
}

impl Log {
    fn line(&mut self, line: impl AsRef<str>) {
        self.push(line.as_ref());
        self.push("\n");
    }

    fn push(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let room = MAX_LOG_BYTES.saturating_sub(self.text.len());
        if text.len() <= room {
            self.text.push_str(text);
        } else {
            let mut cut = room;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            self.text.push_str(&text[..cut]);
            self.text.push_str("\n[log truncated]\n");
            self.truncated = true;
        }
    }
}

/// A job's scratch directory, removed when dropped.
struct JobDir(PathBuf);

impl JobDir {
    fn create(root: &Path, job: Uuid) -> std::io::Result<JobDir> {
        let path = root.join(format!("job-{job}"));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
        set_mode(&path, 0o755)?;
        Ok(JobDir(path))
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(dir = %self.0.display(), error = %err, "could not remove a build directory");
        }
    }
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// How a captured process ended.
struct Captured {
    status: Option<i32>,
    output: Vec<u8>,
    timed_out: bool,
    secs: f64,
}

/// Runs `cmd` with stdout and stderr captured into one buffer (at most
/// `cap` bytes kept), killing it after `timeout`.
async fn run_captured(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    cap: usize,
) -> std::io::Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group: a Ctrl-C at the worker's terminal is the
    // worker's to handle (finish the job), not the build's.
    #[cfg(unix)]
    cmd.process_group(0);
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let pump = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| {
        let buffer = buffer.clone();
        tokio::spawn(async move {
            let Some(mut stream) = stream else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stream.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut out = buffer.lock().unwrap_or_else(|e| e.into_inner());
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..n.min(room)]);
            }
        })
    };
    let out = pump(child.stdout.take().map(|s| Box::new(s) as _));
    let err = pump(child.stderr.take().map(|s| Box::new(s) as _));
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status?.code(), false),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            (None, true)
        }
    };
    // The pipes close with the process (or its container's attach).
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = out.await;
        let _ = err.await;
    })
    .await;
    let output = std::mem::take(&mut *buffer.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(Captured {
        status,
        output,
        timed_out,
        secs: started.elapsed().as_secs_f64(),
    })
}

/// The §8 sandbox command for one build (`--name` added).
pub fn docker_args(name: &str, src: &Path, out: &Path, image: &str) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--rm", "--name", name]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    args.extend(
        [
            "--network",
            "none",
            "--memory",
            "2g",
            "--cpus",
            "1",
            "--pids-limit",
            "256",
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--user",
            "builder",
            "-v",
        ]
        .iter()
        .map(|s| (*s).to_owned()),
    );
    args.push(format!("{}:/src:ro", src.display()));
    args.push("-v".to_owned());
    args.push(format!("{}:/out", out.display()));
    args.push(image.to_owned());
    args.extend(
        ["timeout", "120", "kfm-build"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    args
}

/// What a finished build or validation sets on its submission.
struct Settle<'a> {
    state: &'a str,
    error: Option<String>,
}

pub struct BuildWorker {
    db: PgPool,
    store: Store,
    settings: BuildSettings,
}

impl BuildWorker {
    pub fn new(db: PgPool, store: Store, settings: BuildSettings) -> BuildWorker {
        BuildWorker {
            db,
            store,
            settings,
        }
    }

    /// The submission's state, its team and its keys, or `None`.
    async fn load(&self, id: Uuid) -> Result<Option<Row>, JobError> {
        Ok(sqlx::query_as(
            "SELECT team_id, state, manifest, source_key, module_key, log_key \
             FROM submissions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Stores the log and settles the submission (only from queued or building).
    async fn settle(&self, id: Uuid, log: &Log, settle: Settle<'_>) -> Result<bool, JobError> {
        let key = store::log_key(id);
        self.store
            .put(&key, log.text.as_bytes(), "text/plain; charset=utf-8")
            .await
            .map_err(JobError::retry)?;
        let done = sqlx::query(
            "UPDATE submissions SET state = $2, error = $3, log_key = $4, updated_at = now() \
             WHERE id = $1 AND state IN ('queued', 'building')",
        )
        .bind(id)
        .bind(settle.state)
        .bind(&settle.error)
        .bind(&key)
        .execute(&self.db)
        .await?;
        let settled = done.rows_affected() == 1;
        if settled {
            tracing::info!(submission = %id, state = settle.state, error = ?settle.error, "submission settled");
        }
        Ok(settled)
    }

    async fn fail_build(&self, id: Uuid, log: &mut Log, reason: String) -> Result<(), JobError> {
        log.line(format!("FAILED: {reason}"));
        self.settle(
            id,
            log,
            Settle {
                state: "failed",
                error: Some(reason),
            },
        )
        .await
        .map(|_| ())
    }

    async fn build(&self, job: &Job) -> Result<(), JobError> {
        let id = job
            .submission()
            .ok_or_else(|| JobError::Fail("the payload names no submission".to_owned()))?;
        let Some(row) = self.load(id).await? else {
            return Err(JobError::Fail(format!("no submission {id}")));
        };
        if row.state != "queued" && row.state != "building" {
            tracing::info!(submission = %id, state = %row.state, "already settled; nothing to build");
            return Ok(());
        }
        sqlx::query(
            "UPDATE submissions SET state = 'building', updated_at = now() \
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(id)
        .execute(&self.db)
        .await?;

        let mut log = Log::default();
        log.line(format!(
            "kfm-ladder build of submission {id} (job {}, attempt {}), engine {}",
            job.id, job.attempts, self.settings.engine_tag
        ));
        let manifest = match manifest::parse(&row.manifest) {
            Ok(m) => m,
            Err(e) => return self.fail_build(id, &mut log, e).await,
        };
        log.line(format!(
            "manifest: language = {}, entry = {}",
            manifest.language.as_str(),
            manifest.entry_display()
        ));
        let Some(source_key) = row.source_key else {
            return self
                .fail_build(
                    id,
                    &mut log,
                    "the source archive is no longer stored".to_owned(),
                )
                .await;
        };
        let Some(zip) = self.store.get(&source_key).await.map_err(JobError::retry)? else {
            return Err(JobError::retry(format!(
                "{source_key} is missing from the store"
            )));
        };

        let dir = JobDir::create(&self.settings.build_dir, job.id).map_err(|e| {
            JobError::retry(format!(
                "cannot make a build directory under {}: {e}",
                self.settings.build_dir.display()
            ))
        })?;
        let src = dir.0.join("src");
        let out = dir.0.join("out");
        for (path, mode) in [(&src, 0o755), (&out, 0o777)] {
            std::fs::create_dir(path)
                .and_then(|()| set_mode(path, mode))
                .map_err(|e| JobError::retry(format!("{}: {e}", path.display())))?;
        }
        let entry = manifest.entry.clone();
        let src_for_unpack = src.clone();
        let unpacked =
            tokio::task::spawn_blocking(move || archive::unpack(&zip, &entry, &src_for_unpack))
                .await
                .map_err(JobError::retry)?;
        match unpacked {
            Ok(n) => log.line(format!(
                "unpacked {n} file(s) from {}",
                manifest.entry_display()
            )),
            Err(e) => {
                return self
                    .fail_build(id, &mut log, format!("the source archive is refused: {e}"))
                    .await
            }
        }

        let name = format!("kfm-build-{}", job.id);
        let args = docker_args(&name, &src, &out, &self.settings.image);
        log.line(format!(
            "$ {} {}",
            self.settings.docker.display(),
            args.join(" ")
        ));
        let mut cmd = tokio::process::Command::new(&self.settings.docker);
        cmd.args(&args);
        let run = run_captured(cmd, self.settings.build_timeout, MAX_LOG_BYTES)
            .await
            .map_err(|e| {
                JobError::retry(format!(
                    "cannot run {}: {e}",
                    self.settings.docker.display()
                ))
            })?;
        log.push(&String::from_utf8_lossy(&run.output));
        if !log.text.ends_with('\n') {
            log.line("");
        }
        if run.timed_out {
            // The CLI is gone; the container may not be.
            let _ = tokio::process::Command::new(&self.settings.docker)
                .args(["rm", "-f", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            return self
                .fail_build(
                    id,
                    &mut log,
                    format!(
                        "the build took longer than {} s",
                        self.settings.build_timeout.as_secs()
                    ),
                )
                .await;
        }
        log.line(format!(
            "exit status {} after {:.1} s",
            run.status
                .map_or_else(|| "none (a signal)".to_owned(), |c| c.to_string()),
            run.secs
        ));
        match run.status {
            Some(0) => {}
            // docker's own failures: the daemon, the image, the command.
            Some(code @ (125..=127)) => {
                return Err(JobError::Retry(format!(
                    "docker run exited {code} (the daemon or the image {}): {}",
                    self.settings.image,
                    last_line(&run.output)
                )))
            }
            Some(124) => {
                return self
                    .fail_build(
                        id,
                        &mut log,
                        format!("the build hit the {SANDBOX_SECONDS} s limit"),
                    )
                    .await
            }
            Some(137) => {
                return self
                    .fail_build(
                        id,
                        &mut log,
                        "the build was killed (the 2 GiB memory cap?)".to_owned(),
                    )
                    .await
            }
            Some(code) => {
                return self
                    .fail_build(id, &mut log, format!("the build failed (exit {code})"))
                    .await
            }
            None => {
                return self
                    .fail_build(id, &mut log, "the build was killed by a signal".to_owned())
                    .await
            }
        }
        let module = match std::fs::read(out.join("bot.wasm")) {
            Ok(bytes) => bytes,
            Err(_) => {
                return self
                    .fail_build(id, &mut log, "the build left no bot.wasm".to_owned())
                    .await
            }
        };
        let hash = crypto::sha256(&module);
        log.line(format!(
            "module: {} bytes, sha256 {}",
            module.len(),
            hex::encode(hash)
        ));
        let module_key = store::module_key(id);
        self.store
            .put(&module_key, &module, "application/wasm")
            .await
            .map_err(JobError::retry)?;
        let log_key = store::log_key(id);
        self.store
            .put(&log_key, log.text.as_bytes(), "text/plain; charset=utf-8")
            .await
            .map_err(JobError::retry)?;
        let mut tx = self.db.begin().await?;
        let updated = sqlx::query(
            "UPDATE submissions SET module_key = $2, module_hash = $3, log_key = $4, \
             updated_at = now() WHERE id = $1 AND state = 'building'",
        )
        .bind(id)
        .bind(&module_key)
        .bind(&hash[..])
        .bind(&log_key)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 1 {
            jobs::enqueue(
                &mut tx,
                Kind::Validate,
                serde_json::json!({ "submission": id }),
            )
            .await?;
        }
        tx.commit().await?;
        tracing::info!(submission = %id, bytes = module.len(), secs = run.secs, "module built");
        Ok(())
    }

    async fn reject(&self, id: Uuid, log: &mut Log, reason: String) -> Result<(), JobError> {
        log.line(format!("REJECTED: {reason}"));
        self.settle(
            id,
            log,
            Settle {
                state: "rejected",
                error: Some(reason),
            },
        )
        .await
        .map(|_| ())
    }

    async fn validate(&self, job: &Job) -> Result<(), JobError> {
        let id = job
            .submission()
            .ok_or_else(|| JobError::Fail("the payload names no submission".to_owned()))?;
        let Some(row) = self.load(id).await? else {
            return Err(JobError::Fail(format!("no submission {id}")));
        };
        if row.state != "building" {
            tracing::info!(submission = %id, state = %row.state, "already settled; nothing to validate");
            return Ok(());
        }
        let module_key = row
            .module_key
            .ok_or_else(|| JobError::Fail(format!("submission {id} has no module")))?;
        let module = self
            .store
            .get(&module_key)
            .await
            .map_err(JobError::retry)?
            .ok_or_else(|| JobError::retry(format!("{module_key} is missing from the store")))?;
        let mut log = Log::default();
        if let Some(key) = &row.log_key {
            if let Some(text) = self.store.get(key).await.map_err(JobError::retry)? {
                log.push(&String::from_utf8_lossy(&text));
            }
        }
        log.line("");
        log.line(format!(
            "kfm-ladder validation (job {}, attempt {}), kfm_arena {}",
            job.id, job.attempts, self.settings.engine_tag
        ));
        match wasm::check(&module) {
            Ok(info) => log.line(format!(
                "module checks: {} bytes, {} initial page(s), imports {}, exports {}",
                info.bytes,
                info.initial_pages,
                if info.imports_log {
                    "env.kfm_log"
                } else {
                    "nothing"
                },
                info.exports.join(" ")
            )),
            Err(reason) => {
                return self
                    .reject(id, &mut log, format!("the module is refused: {reason}"))
                    .await
            }
        }

        let dir = JobDir::create(&self.settings.build_dir, job.id).map_err(JobError::retry)?;
        let module_path = dir.0.join("bot.wasm");
        let result_path = dir.0.join("result.jsonl");
        std::fs::write(&module_path, &module).map_err(JobError::retry)?;
        let run = arena::smoke(
            &self.settings.arena,
            SMOKE_MODE,
            &module_path,
            &result_path,
            self.settings.smoke_timeout,
        )
        .await
        .map_err(JobError::Retry)?;
        log.line(format!("$ {}", run.command));
        if !run.stderr.is_empty() {
            log.push(&run.stderr);
            if !run.stderr.ends_with('\n') {
                log.line("");
            }
        }
        match run.status {
            Some(0) => {}
            // The host refused the module at load (docs/arena-result.md §4).
            Some(3) => {
                let why = last_line(run.stderr.as_bytes());
                return self
                    .reject(id, &mut log, format!("kfm_arena refused the module: {why}"))
                    .await;
            }
            other => {
                return Err(JobError::Retry(format!(
                    "kfm_arena exited {other:?}: {}",
                    last_line(run.stderr.as_bytes())
                )))
            }
        }
        let result = std::fs::read_to_string(&result_path)
            .map_err(|e| JobError::retry(format!("the smoke match left no result: {e}")))?;
        log.push(&result);
        match arena::judge(&result)
            .map_err(|e| JobError::Retry(format!("the smoke match's result: {e}")))?
        {
            arena::Verdict::Clean(summary) => {
                log.line(format!("smoke match: {summary}"));
                log.line("READY");
                self.settle(
                    id,
                    &log,
                    Settle {
                        state: "ready",
                        error: None,
                    },
                )
                .await?;
                tracing::info!(submission = %id, team = %row.team_id, "module ready");
                // A failed prune is retried by the next ready submission;
                // it never undoes this one.
                if let Err(err) = retention::prune_team(&self.db, &self.store, row.team_id).await {
                    tracing::warn!(team = %row.team_id, error = %err, "retention failed");
                }
                Ok(())
            }
            arena::Verdict::Faulty(faults) => {
                for fault in &faults {
                    log.line(format!("fault: {fault}"));
                }
                let reason = format!("the smoke match found faults: {}", faults.join("; "));
                self.reject(id, &mut log, reason).await
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    team_id: Uuid,
    state: String,
    manifest: String,
    source_key: Option<String>,
    module_key: Option<String>,
    log_key: Option<String>,
}

/// The last non-empty line of a process's output, for an error message.
fn last_line(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    line.chars().take(300).collect()
}

impl Handler for BuildWorker {
    fn kinds(&self) -> &[Kind] {
        &[Kind::Build, Kind::Validate]
    }

    async fn run(&self, job: &Job) -> Result<(), JobError> {
        match Kind::parse(&job.kind) {
            Some(Kind::Build) => self.build(job).await,
            Some(Kind::Validate) => self.validate(job).await,
            _ => Err(JobError::Fail(format!(
                "the build worker does not run {} jobs",
                job.kind
            ))),
        }
    }

    async fn gave_up(&self, kind: &str, payload: &Value, error: &str) {
        let Some(id) = payload
            .get("submission")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return;
        };
        let reason = format!("the {kind} could not be completed: {error}");
        let mut log = Log::default();
        if let Ok(Some(row)) = self.load(id).await {
            if let Some(key) = row.log_key {
                if let Ok(Some(text)) = self.store.get(&key).await {
                    log.push(&String::from_utf8_lossy(&text));
                }
            }
        }
        log.line(format!("FAILED: {reason}"));
        let settled = self
            .settle(
                id,
                &log,
                Settle {
                    state: "failed",
                    error: Some(reason.clone()),
                },
            )
            .await;
        if let Err(err) = settled {
            // Without the store, the row still must not stay `building`.
            tracing::warn!(submission = %id, error = %err, "could not store the log of a job given up");
            let _ = sqlx::query(
                "UPDATE submissions SET state = 'failed', error = $2, updated_at = now() \
                 WHERE id = $1 AND state IN ('queued', 'building')",
            )
            .bind(id)
            .bind(&reason)
            .execute(&self.db)
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_command_has_the_flags_of_section_8() {
        let args = docker_args(
            "kfm-build-x",
            Path::new("/b/job/src"),
            Path::new("/b/job/out"),
            "kfm-bot-build:v1",
        );
        assert_eq!(
            args.join(" "),
            "run --rm --name kfm-build-x --network none --memory 2g --cpus 1 --pids-limit 256 \
             --read-only --tmpfs /tmp --user builder -v /b/job/src:/src:ro -v /b/job/out:/out \
             kfm-bot-build:v1 timeout 120 kfm-build"
        );
    }

    #[test]
    fn logs_are_capped() {
        let mut log = Log::default();
        log.push(&"x".repeat(MAX_LOG_BYTES - 2));
        log.line("ééé");
        assert!(log.truncated);
        assert!(log.text.ends_with("[log truncated]\n"));
        assert!(log.text.len() <= MAX_LOG_BYTES + 20);
        log.line("more");
        assert!(!log.text.ends_with("more\n"));
    }

    #[test]
    fn worker_build_needs_its_three_variables() {
        let base = WorkerConfig {
            database_url: "postgres://x/y".to_owned(),
            s3: crate::config::S3Config {
                endpoint: "http://127.0.0.1:9000".parse().unwrap(),
                bucket: "b".to_owned(),
                access_key: "a".to_owned(),
                secret_key: "s".to_owned(),
                region: "us-east-1".to_owned(),
            },
            engine_tag: "v0.5.1".to_owned(),
            arena: Some("/opt/kfm/engine/kfm_arena".into()),
            build_image: Some("kfm-bot-build:dev".to_owned()),
            build_dir: Some("/var/lib/kfm-ladder/build".into()),
            sentry_dsn: None,
        };
        assert!(BuildSettings::from_config(&base).is_ok());
        for (variable, edit) in [
            ("KFM_BUILD_IMAGE", 0),
            ("KFM_LADDER_BUILD_DIR", 1),
            ("KFM_LADDER_ARENA", 2),
            ("KFM_LADDER_BUILD_DIR", 3),
        ] {
            let mut config = base.clone();
            match edit {
                0 => config.build_image = None,
                1 => config.build_dir = None,
                2 => config.arena = None,
                _ => config.build_dir = Some("relative/build".into()),
            }
            assert_eq!(
                BuildSettings::from_config(&config).unwrap_err().variable,
                variable
            );
        }
    }
}
