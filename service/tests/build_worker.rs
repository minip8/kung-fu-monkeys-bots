//! The build path end to end: an upload through the API, the build worker's
//! loop claiming its build in a real sandbox container (`docker run` of the
//! bot build image with the §8 flags), validation with the released
//! kfm_arena, and the submission ready, failed or rejected with its log.
//!
//! Needs Postgres (DATABASE_URL), a MinIO (KFM_LADDER_TEST_S3_ENDPOINT),
//! Docker with the bot build image (KFM_LADDER_TEST_BUILD_IMAGE, default
//! kfm-bot-build:local: `docker build -t kfm-bot-build:local .`) and kfm_arena
//! (KFM_LADDER_ARENA, default ../.kfm/kfm_arena: tools/fetch_kfm_arena.sh).
//! Each test says what is missing and passes without running when one is.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{
    age_submissions, bearer_json, github_browser, have_store, new_team, new_token, start, upload,
    zip_dir, zip_of, TestApp,
};
use kfm_ladder::builder::{BuildSettings, BuildWorker};
use kfm_ladder::worker::{self, Timing};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::watch;

fn image() -> String {
    std::env::var("KFM_LADDER_TEST_BUILD_IMAGE")
        .unwrap_or_else(|_| "kfm-bot-build:local".to_owned())
}

fn arena() -> PathBuf {
    std::env::var("KFM_LADDER_ARENA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.kfm/kfm_arena"))
        })
}

/// Whether Docker answers with the build image present and kfm_arena exists.
fn have_sandbox(test: &str) -> bool {
    if !have_store(test) {
        return false;
    }
    let image = image();
    let docker = std::process::Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", &image])
        .output();
    match docker {
        Ok(out) if out.status.success() => {}
        Ok(_) => {
            eprintln!("{test}: skipped: the image {image} is missing (docker build -t {image} .)");
            return false;
        }
        Err(e) => {
            eprintln!("{test}: skipped: docker is not reachable ({e})");
            return false;
        }
    }
    if !arena().is_file() {
        eprintln!(
            "{test}: skipped: no kfm_arena at {} (tools/fetch_kfm_arena.sh, or set KFM_LADDER_ARENA)",
            arena().display()
        );
        return false;
    }
    true
}

fn settings(build_dir: PathBuf) -> BuildSettings {
    BuildSettings {
        image: image(),
        build_dir,
        arena: arena(),
        engine_tag: "v0.5.1".to_owned(),
        docker: PathBuf::from("docker"),
        build_timeout: kfm_ladder::builder::BUILD_TIMEOUT,
        smoke_timeout: kfm_ladder::arena::SMOKE_TIMEOUT,
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

/// A build worker over the test app's database and store, until stopped.
struct RunningWorker {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    dir: PathBuf,
}

impl RunningWorker {
    fn start(app: &TestApp, settings_for: impl FnOnce(PathBuf) -> BuildSettings) -> RunningWorker {
        let dir =
            std::env::temp_dir().join(format!("kfm-ladder-build-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let handler =
            BuildWorker::new(app.db.clone(), app.store.clone(), settings_for(dir.clone()));
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(worker::run(
            app.db.clone(),
            handler,
            worker::worker_name("test"),
            timing(),
            rx,
        ));
        RunningWorker { stop, task, dir }
    }

    async fn stop(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(200), self.task)
            .await
            .unwrap()
            .unwrap();
        // Every job directory was removed.
        assert_eq!(std::fs::read_dir(&self.dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

/// Waits until the submission leaves queued and building; its state.
async fn settled(db: &PgPool, id: &str) -> String {
    for _ in 0..1200 {
        let (state,): (String,) =
            sqlx::query_as("SELECT state FROM submissions WHERE id = $1::uuid")
                .bind(id)
                .fetch_one(db)
                .await
                .unwrap();
        if state != "queued" && state != "building" {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("submission {id} never settled");
}

async fn submit(app: &TestApp, token: &str, team: &str, manifest: &str, zip: Vec<u8>) -> String {
    let response = upload(app, token, team, manifest, zip).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let sub: Value = response.json().await.unwrap();
    sub["id"].as_str().unwrap().to_owned()
}

async fn show(app: &TestApp, token: &str, id: &str) -> Value {
    bearer_json(app, Method::GET, &format!("/submissions/{id}"), token, None)
        .await
        .json()
        .await
        .unwrap()
}

async fn log_of(app: &TestApp, token: &str, id: &str) -> String {
    let response = bearer_json(
        app,
        Method::GET,
        &format!("/submissions/{id}/log"),
        token,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    response.text().await.unwrap()
}

async fn owner(app: &TestApp, id: u64) -> (String, String) {
    let mut browser = github_browser(app, id, &format!("builder{id}")).await;
    let team = new_team(&mut browser, &format!("Builders {id}")).await;
    let token = new_token(&mut browser, "submit").await;
    (team, token)
}

#[sqlx::test]
async fn the_starter_example_builds_validates_and_is_ready(pool: PgPool) {
    if !have_sandbox("the_starter_example_builds_validates_and_is_ready") {
        return;
    }
    let app = start(pool).await;
    let (team, token) = owner(&app, 7001).await;
    let started = std::time::Instant::now();
    // The example under a directory, named by the manifest's entry.
    let id = submit(
        &app,
        &token,
        &team,
        "language = \"cpp\"\nentry = \"starter\"\n",
        zip_dir("examples/starter", "starter/"),
    )
    .await;
    let running = RunningWorker::start(&app, settings);
    assert_eq!(settled(&app.db, &id).await, "ready");
    eprintln!(
        "the starter example: upload to ready in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    running.stop().await;

    let sub = show(&app, &token, &id).await;
    assert_eq!(sub["state"], "ready");
    assert_eq!(sub["error"], Value::Null);
    let module = app
        .store
        .get(&format!("modules/{id}.wasm"))
        .await
        .unwrap()
        .expect("the module is stored");
    assert!(kfm_ladder::wasm::check(&module).is_ok());
    assert_eq!(
        sub["module_hash"],
        hex::encode(kfm_ladder::crypto::sha256(&module))
    );
    let log = log_of(&app, &token, &id).await;
    for needle in [
        "docker run --rm --name kfm-build-",
        "--network none --memory 2g --cpus 1 --pids-limit 256 --read-only --tmpfs /tmp --user builder",
        "timeout 120 kfm-build",
        "exit status 0",
        "module checks:",
        "--mode contest --team",
        "smoke match: 1 match(es), 1 module slot(s)",
        "READY",
    ] {
        assert!(log.contains(needle), "{needle:?} not in the log:\n{log}");
    }
    // The presigned URL serves the same log.
    let url = sub["log_url"].as_str().unwrap();
    assert_eq!(reqwest::get(url).await.unwrap().text().await.unwrap(), log);
    let (jobs,): (Vec<String>,) =
        sqlx::query_as("SELECT array_agg(kind || ':' || state ORDER BY created_at) FROM jobs")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(jobs, ["build:done", "validate:done"]);

    // Ready, so it can be made active.
    let response = bearer_json(
        &app,
        Method::POST,
        &format!("/teams/{team}/active"),
        &token,
        Some(serde_json::json!({ "submission": id })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[sqlx::test]
async fn a_compile_error_fails_with_the_compilers_log(pool: PgPool) {
    if !have_sandbox("a_compile_error_fails_with_the_compilers_log") {
        return;
    }
    let app = start(pool).await;
    let (team, token) = owner(&app, 7002).await;
    let id = submit(
        &app,
        &token,
        &team,
        "language = \"c\"\n",
        zip_of(&[("bot.c", b"int kfm_act(int tick) { return tick + ; }\n")]),
    )
    .await;
    let running = RunningWorker::start(&app, settings);
    assert_eq!(settled(&app.db, &id).await, "failed");
    running.stop().await;
    let sub = show(&app, &token, &id).await;
    assert_eq!(sub["error"], "the build failed (exit 1)");
    assert_eq!(sub["module_hash"], Value::Null);
    let log = log_of(&app, &token, &id).await;
    assert!(log.contains("bot.c:1:"), "{log}");
    assert!(log.contains("error:"), "{log}");
    assert!(log.contains("FAILED: the build failed (exit 1)"), "{log}");
    assert_eq!(
        app.store.get(&format!("modules/{id}.wasm")).await.unwrap(),
        None
    );
}

/// A bot that satisfies every load check and traps on every act.
const TRAPPING_BOT: &[u8] = br#"
static unsigned char obs[5748] __attribute__((aligned(4)));
int kfm_abi_version(void) { return 1; }
int kfm_obs_buffer(void) { return (int)(unsigned long)obs; }
void kfm_init(unsigned long long seed, int slot) { (void)seed; (void)slot; }
long long kfm_act(int tick) { (void)tick; __builtin_trap(); }
"#;

#[sqlx::test]
async fn a_module_that_traps_is_rejected(pool: PgPool) {
    if !have_sandbox("a_module_that_traps_is_rejected") {
        return;
    }
    let app = start(pool).await;
    let (team, token) = owner(&app, 7003).await;
    let id = submit(
        &app,
        &token,
        &team,
        "language = \"c\"\n",
        zip_of(&[("trap.c", TRAPPING_BOT)]),
    )
    .await;
    let running = RunningWorker::start(&app, settings);
    assert_eq!(settled(&app.db, &id).await, "rejected");
    running.stop().await;
    let sub = show(&app, &token, &id).await;
    let error = sub["error"].as_str().unwrap();
    assert!(
        error.starts_with("the smoke match found faults: match 0 slot 0: traps "),
        "{error}"
    );
    // The module was built and is kept (with its hash), but never ready.
    assert!(sub["module_hash"].is_string());
    let log = log_of(&app, &token, &id).await;
    assert!(log.contains("exit status 0"), "{log}");
    assert!(
        log.contains("REJECTED: the smoke match found faults"),
        "{log}"
    );
    let response = bearer_json(
        &app,
        Method::POST,
        &format!("/teams/{team}/active"),
        &token,
        Some(serde_json::json!({ "submission": id })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[sqlx::test]
async fn a_build_that_cannot_run_fails_after_its_attempts(pool: PgPool) {
    if !have_store("a_build_that_cannot_run_fails_after_its_attempts") {
        return;
    }
    let app = start(pool).await;
    let (team, token) = owner(&app, 7004).await;
    let id = submit(
        &app,
        &token,
        &team,
        "language = \"c\"\n",
        zip_of(&[("bot.c", b"int x;")]),
    )
    .await;
    let running = RunningWorker::start(&app, |dir| BuildSettings {
        docker: PathBuf::from("/nonexistent/docker"),
        ..settings(dir)
    });
    assert_eq!(settled(&app.db, &id).await, "failed");
    running.stop().await;
    let (state, attempts): (String, i32) = sqlx::query_as("SELECT state, attempts FROM jobs")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("failed", 3));
    let sub = show(&app, &token, &id).await;
    let error = sub["error"].as_str().unwrap();
    assert!(
        error.starts_with("the build could not be completed: cannot run /nonexistent/docker"),
        "{error}"
    );
    assert!(log_of(&app, &token, &id)
        .await
        .contains("FAILED: the build could not be completed"));
    // The team may submit again once the minute is over.
    age_submissions(&app.db, &team).await;
    let again = upload(
        &app,
        &token,
        &team,
        "language = \"c\"\n",
        zip_of(&[("bot.c", b"int x;")]),
    )
    .await;
    assert_eq!(again.status(), StatusCode::ACCEPTED);
}

#[sqlx::test]
async fn the_sixth_ready_submission_prunes_the_oldest(pool: PgPool) {
    if !have_sandbox("the_sixth_ready_submission_prunes_the_oldest") {
        return;
    }
    let app = start(pool).await;
    let (team, token) = owner(&app, 7005).await;
    // Five finished submissions, oldest first, each with its objects.
    let mut old = Vec::new();
    for minutes in (2..7).rev() {
        let id = uuid::Uuid::now_v7();
        let (source, log) = (format!("sources/{id}.zip"), format!("logs/{id}.txt"));
        app.store
            .put(&source, b"zip", "application/zip")
            .await
            .unwrap();
        app.store.put(&log, b"log", "text/plain").await.unwrap();
        sqlx::query(
            "INSERT INTO submissions (id, team_id, manifest, source_key, log_key, state, created_at) \
             VALUES ($1, $2::uuid, 'language = \"c\"', $3, $4, 'failed', now() - make_interval(mins => $5))",
        )
        .bind(id)
        .bind(&team)
        .bind(&source)
        .bind(&log)
        .bind(minutes)
        .execute(&app.db)
        .await
        .unwrap();
        old.push(id);
    }
    let id = submit(
        &app,
        &token,
        &team,
        "language = \"c\"\n",
        zip_dir("examples/idle", ""),
    )
    .await;
    let running = RunningWorker::start(&app, settings);
    assert_eq!(settled(&app.db, &id).await, "ready");
    running.stop().await;
    let kept: Vec<(uuid::Uuid, bool)> =
        sqlx::query_as("SELECT id, source_key IS NOT NULL FROM submissions ORDER BY created_at")
            .fetch_all(&app.db)
            .await
            .unwrap();
    let pruned: Vec<_> = kept
        .iter()
        .filter(|(_, has)| !has)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(pruned, [old[0]], "the oldest of six finished goes");
    assert_eq!(
        app.store
            .get(&format!("sources/{}.zip", old[0]))
            .await
            .unwrap(),
        None
    );
    assert!(app
        .store
        .get(&format!("sources/{}.zip", old[1]))
        .await
        .unwrap()
        .is_some());
}
