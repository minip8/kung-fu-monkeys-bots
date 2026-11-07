//! The test harness: a fake ladder (axum, in process, on an ephemeral port)
//! serving the routes of docs/ladder-api.md that `kfm` calls, with canned
//! JSON, and a runner for the real `kfm` binary against it with its own
//! config file and cache.

#![allow(dead_code)]
// A fake's handlers return the response they would send, boxed or not.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Multipart, Path as UrlPath, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// The token of a user in one team, and of a user in two.
pub const TOKEN: &str = "kfm_one_team_token_0000000000000000000000000000";
pub const TOKEN2: &str = "kfm_two_team_token_0000000000000000000000000000";
pub const TOKEN_ID: &str = "0190f0f0-1c2d-7a00-8000-00000000a0a0";
pub const CODE: &str = "good-code-22-characters";
pub const TEAM_A: &str = "0190f0f0-1c2d-7a00-8000-0000000000a1";
pub const TEAM_B: &str = "0190f0f0-1c2d-7a00-8000-0000000000b2";
/// A submission that turns ready, and one whose build fails.
pub const SUB_OK: &str = "0190f0f0-1c2d-7a00-8000-000000000001";
pub const SUB_FAIL: &str = "0190f0f0-1c2d-7a00-8000-000000000002";
pub const MATCH: &str = "0190f0f0-1c2d-7a00-8000-0000000000e1";
pub const REPLAY: &[u8] = b"KFMR replay bytes";

/// What a fake upload received.
#[derive(Debug, Clone)]
pub struct Upload {
    pub team: String,
    pub manifest: String,
    pub manifest_file: Option<String>,
    pub zip: Vec<u8>,
    pub zip_file: Option<String>,
    pub zip_type: Option<String>,
}

#[derive(Default)]
pub struct Seen {
    pub uploads: Vec<Upload>,
    pub polls: HashMap<String, usize>,
    pub exchanged: Vec<String>,
    pub revoked: Vec<String>,
    pub activated: Vec<(String, String)>,
    /// The Authorization header of every replay download.
    pub replay_auth: Vec<Option<String>>,
    /// Paths with their query, in order.
    pub requests: Vec<String>,
}

#[derive(Clone)]
pub struct Fake {
    pub base: String,
    pub seen: Arc<Mutex<Seen>>,
}

fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The caller's token, or the API's 401.
fn caller(headers: &HeaderMap) -> Result<&'static str, Response> {
    match bearer(headers) {
        None => Err(err(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Sign in or send a bearer token.",
        )),
        Some(TOKEN) => Ok(TOKEN),
        Some(TOKEN2) => Ok(TOKEN2),
        Some(_) => Err(err(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "The bearer token is unknown or revoked.",
        )),
    }
}

fn record(fake: &Fake, what: String) {
    fake.seen.lock().unwrap().requests.push(what);
}

fn submission(id: &str, state: &str) -> Value {
    let failed = state == "failed";
    json!({
        "id": id, "team_id": TEAM_A, "submitted_by": null,
        "created_at": "2026-11-07T09:28:09.266849Z", "updated_at": "2026-11-07T09:29:00Z",
        "state": state, "language": "cpp", "entry": ".",
        "module_hash": if state == "ready" { json!("ab".repeat(32)) } else { Value::Null },
        "log_url": null,
        "error": if failed { json!("the compile failed") } else { Value::Null },
        "active": false, "ranked_games": 0, "ranked_faults": 0
    })
}

pub fn match_summary(fake: &Fake) -> Value {
    json!({
        "id": MATCH, "season": "2026-12", "season_id": "0190f0f0-1c2d-7a00-8000-0000000000s1",
        "round": 4, "game": 1, "created_at": "2026-12-03T10:00:00Z", "engine_tag": "v0.5.1",
        "seed": 1234, "ticks": 1800, "end": "ended", "winner": TEAM_B,
        "sides": [
            {"team_id": TEAM_A, "name": "Banana Squad", "submission_id": SUB_OK, "team": 1,
             "score": 1, "kills": 4, "deaths": 9, "rating_change": -12.3,
             "faults": [{"slot": 2, "traps": 1, "over": 0, "bad": 0, "dead": null}]},
            {"team_id": TEAM_B, "name": "Peel Team", "submission_id": SUB_FAIL, "team": 0,
             "score": 3, "kills": 9, "deaths": 4, "rating_change": 12.3, "faults": []}
        ],
        "content_hash": "0c0ffee1", "build_hash": null,
        "result": {"match": 1},
        "replay_url": format!("{}/store/replays/{MATCH}.kfmr?X-Amz-Signature=abc", fake.base),
        "viewer_url": format!("https://kfm.minip8.com/v0.5.1/?replay={}/store", fake.base),
    })
}

/// Starts the fake ladder on its own runtime thread.
pub fn start() -> Fake {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fake = Fake {
                base: format!("http://{}", listener.local_addr().unwrap()),
                seen: Arc::default(),
            };
            tx.send(fake.clone()).unwrap();
            axum::serve(listener, router(fake)).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

fn router(fake: Fake) -> Router {
    Router::new()
        .route("/auth/cli/exchange", post(exchange))
        .route("/me", get(me))
        .route("/teams", get(teams))
        .route("/tokens/{id}", delete(revoke))
        .route(
            "/teams/{id}/submissions",
            post(upload).get(list_submissions),
        )
        .route("/teams/{id}/active", post(activate))
        .route("/submissions/{id}", get(show_submission))
        .route("/submissions/{id}/log", get(log))
        .route("/matches", get(matches))
        .route("/matches/{id}", get(show_match))
        .route("/ladder", get(ladder))
        .route("/seasons", get(seasons))
        .route("/store/replays/{name}", get(replay))
        .with_state(fake)
}

async fn exchange(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let code = body["code"].as_str().unwrap_or_default().to_owned();
    fake.seen.lock().unwrap().exchanged.push(code.clone());
    if code != CODE {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_code",
            "The login code is unknown, used or expired; run kfm login again.",
        );
    }
    Json(json!({"token": TOKEN, "token_id": TOKEN_ID, "name": "kfm on testbox", "scope": "submit"}))
        .into_response()
}

async fn me(headers: HeaderMap) -> Response {
    let token = match caller(&headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let name = if token == TOKEN {
        "octomonkey"
    } else {
        "twoteams"
    };
    Json(json!({
        "id": "0190f0f0-1c2d-7a00-8000-0000000000u1", "display_name": name,
        "created_at": "2026-11-01T00:00:00Z", "identities": [], "auth": "token", "scope": "submit"
    }))
    .into_response()
}

async fn teams(headers: HeaderMap) -> Response {
    let token = match caller(&headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let a = json!({"id": TEAM_A, "name": "Banana Squad", "role": "owner", "created_at": "2026-11-01T00:00:00Z"});
    let b = json!({"id": TEAM_B, "name": "Peel Team", "role": "member", "created_at": "2026-11-02T00:00:00Z"});
    let list = if token == TOKEN { vec![a] } else { vec![a, b] };
    Json(json!({ "teams": list })).into_response()
}

async fn revoke(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    if id != TOKEN_ID {
        return err(
            StatusCode::FORBIDDEN,
            "session_required",
            "This route needs a session.",
        );
    }
    fake.seen.lock().unwrap().revoked.push(id);
    StatusCode::NO_CONTENT.into_response()
}

fn zip_names(zip: &[u8]) -> Vec<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
        .collect()
}

async fn upload(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(team): UrlPath<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    let mut manifest = None;
    let mut zip = None;
    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().unwrap_or_default().to_owned();
        let file = field.file_name().map(str::to_owned);
        let ctype = field.content_type().map(str::to_owned);
        let data = field.bytes().await.unwrap().to_vec();
        match name.as_str() {
            "manifest.toml" => manifest = Some((String::from_utf8(data).unwrap(), file)),
            "source.zip" => zip = Some((data, file, ctype)),
            _ => {}
        }
    }
    let (Some((manifest, manifest_file)), Some((zip, zip_file, zip_type))) = (manifest, zip) else {
        return err(StatusCode::BAD_REQUEST, "bad_source", "A part is missing.");
    };
    let fail = zip_names(&zip).iter().any(|n| n.contains("broken"));
    fake.seen.lock().unwrap().uploads.push(Upload {
        team,
        manifest,
        manifest_file,
        zip,
        zip_file,
        zip_type,
    });
    let id = if fail { SUB_FAIL } else { SUB_OK };
    (StatusCode::ACCEPTED, Json(submission(id, "queued"))).into_response()
}

async fn list_submissions(headers: HeaderMap, UrlPath(team): UrlPath<String>) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    if team != TEAM_A {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "You are not allowed to do this.",
        );
    }
    let mut ready = submission(SUB_OK, "ready");
    ready["active"] = json!(true);
    Json(json!({ "submissions": [ready, submission(SUB_FAIL, "failed")] })).into_response()
}

async fn show_submission(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    let n = {
        let mut seen = fake.seen.lock().unwrap();
        let n = seen.polls.entry(id.clone()).or_default();
        *n += 1;
        *n
    };
    let state = match (id.as_str(), n) {
        (_, 1) => "building",
        (SUB_OK, _) => "ready",
        (SUB_FAIL, _) => "failed",
        _ => return err(StatusCode::NOT_FOUND, "not_found", "Not found."),
    };
    Json(submission(&id, state)).into_response()
}

async fn log(headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    if id != SUB_FAIL && id != SUB_OK {
        return err(StatusCode::NOT_FOUND, "not_found", "Not found.");
    }
    (
        [("content-type", "text/plain; charset=utf-8")],
        "+ clang++ ... bot.cpp\nbot.cpp:1:1: error: expected unqualified-id\nFAILED: the compile failed\n",
    )
        .into_response()
}

async fn activate(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(team): UrlPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = caller(&headers) {
        return r;
    }
    let sub = body["submission"].as_str().unwrap_or_default().to_owned();
    fake.seen
        .lock()
        .unwrap()
        .activated
        .push((team, sub.clone()));
    let mut s = submission(&sub, "ready");
    s["active"] = json!(true);
    Json(s).into_response()
}

async fn matches(State(fake): State<Fake>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut keys: Vec<_> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
    keys.sort();
    record(&fake, format!("/matches?{}", keys.join("&")));
    let mut m = match_summary(&fake);
    for k in [
        "content_hash",
        "build_hash",
        "result",
        "replay_url",
        "viewer_url",
    ] {
        m.as_object_mut().unwrap().remove(k);
    }
    Json(json!({ "page": 1, "matches": [m], "next": 2 })).into_response()
}

async fn show_match(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(accept.contains("application/json"), "accept {accept}");
    if id != MATCH {
        return err(StatusCode::NOT_FOUND, "not_found", "Not found.");
    }
    Json(match_summary(&fake)).into_response()
}

async fn ladder(Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("season").is_some_and(|s| s != "2026-12") {
        return err(StatusCode::NOT_FOUND, "not_found", "Not found.");
    }
    Json(json!({
        "season": {"id": "0190f0f0-1c2d-7a00-8000-0000000000s1", "name": "2026-12", "mode": "contest",
                   "starts_at": "2026-12-01T00:00:00Z", "ends_at": "2027-01-01T00:00:00Z", "current": true},
        "page": 1,
        "ratings": [
            {"rank": 1, "team_id": TEAM_B, "name": "Peel Team", "rating": 1712, "rd": 64, "games": 40,
             "wins": 28, "draws": 3, "losses": 9, "active": SUB_FAIL, "module_hash": "cd".repeat(32)},
            {"rank": 2, "team_id": TEAM_A, "name": "Banana Squad", "rating": 1490, "rd": 80, "games": 40,
             "wins": 9, "draws": 3, "losses": 28, "active": SUB_OK, "module_hash": "ab".repeat(32)}
        ],
        "next": null
    }))
    .into_response()
}

async fn seasons() -> Response {
    Json(json!({
        "seasons": [
            {"id": "0190f0f0-1c2d-7a00-8000-0000000000s1", "name": "2026-12", "mode": "contest",
             "starts_at": "2026-12-01T00:00:00Z", "ends_at": "2027-01-01T00:00:00Z", "current": true}
        ],
        "engine_tag": "v0.5.1"
    }))
    .into_response()
}

async fn replay(State(fake): State<Fake>, headers: HeaderMap, _body: Bytes) -> Response {
    let auth = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    fake.seen.lock().unwrap().replay_auth.push(auth);
    REPLAY.to_vec().into_response()
}

/// A scratch home for one run of `kfm`: its config file and cache.
pub struct Home {
    pub dir: tempfile::TempDir,
}

impl Home {
    pub fn new() -> Home {
        Home {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn config(&self) -> PathBuf {
        self.dir.path().join("config/config.toml")
    }

    /// A config file holding `token` for the fake.
    pub fn logged_in(fake: &Fake, token: &str) -> Home {
        let home = Home::new();
        std::fs::create_dir_all(home.config().parent().unwrap()).unwrap();
        std::fs::write(
            home.config(),
            format!(
                "server = \"{}\"\ntoken = \"{token}\"\ntoken_id = \"{TOKEN_ID}\"\ntoken_name = \"kfm on testbox\"\n",
                fake.base
            ),
        )
        .unwrap();
        home
    }

    pub fn command(&self, fake: &Fake, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kfm"));
        cmd.args(args)
            .env("KFM_CONFIG", self.config())
            .env("KFM_CACHE_DIR", self.dir.path().join("cache"))
            .env("KFM_SERVER", &fake.base)
            .env("KFM_POLL_INTERVAL_MS", "10")
            .env_remove("KFM_TOKEN")
            .env_remove("KFM_BUILD_IMAGE")
            .env_remove("KFM_ARENA");
        cmd
    }

    pub fn run(&self, fake: &Fake, args: &[&str]) -> Output {
        self.command(fake, args).output().unwrap()
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asserts the exit code, showing the output when it differs.
pub fn expect(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
}

/// A bot directory with `files` in a temporary directory.
pub fn bot_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

/// The names of a zip's entries.
pub fn names(zip: &[u8]) -> Vec<String> {
    zip_names(zip)
}

/// A file of a zip.
pub fn zip_file(zip: &[u8], name: &str) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut file = archive.by_name(name).unwrap();
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    out
}

/// The repository's root.
pub fn repo() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}
