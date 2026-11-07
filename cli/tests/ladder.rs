//! The real `kfm` binary against the fake ladder of `common`: login and the
//! stored token, whoami, logout, submit (the multipart upload, the zip rules,
//! the states under --wait, activation), the listings and replays, and the
//! error shape and exit codes.

mod common;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use common::*;
use serde_json::Value;

/// Starts `kfm login --no-browser`; the child (its caller waits for it) and the URL it printed.
#[allow(clippy::zombie_processes)]
fn start_login(
    fake: &Fake,
    home: &Home,
) -> (
    std::process::Child,
    url::Url,
    BufReader<std::process::ChildStderr>,
) {
    let mut child = home
        .command(fake, &["login", "--no-browser"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "kfm login printed no URL"
        );
        if let Some(at) = line.find("http") {
            let url = url::Url::parse(line[at..].trim()).unwrap();
            if url.path() == "/auth/cli" {
                return (child, url, reader);
            }
        }
    }
}

fn param(url: &url::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

#[test]
fn login_exchanges_the_code_and_stores_the_token_owner_only() {
    let fake = start();
    let home = Home::new();
    let (mut child, url, _err) = start_login(&fake, &home);
    assert!(
        url.as_str()
            .starts_with(&format!("{}/auth/cli?", fake.base)),
        "{url}"
    );
    let port: u16 = param(&url, "port").unwrap().parse().unwrap();
    let state = param(&url, "state").unwrap();
    assert_eq!(state.len(), 64);
    assert!(param(&url, "host").is_some_and(|h| !h.is_empty()));

    // A stranger's guess and other paths get a 404; the listener keeps waiting.
    let callback = format!("http://127.0.0.1:{port}/callback");
    let wrong = http()
        .get(format!("{callback}?state=nope&code={CODE}"))
        .send()
        .unwrap();
    assert_eq!(wrong.status(), 404);
    let favicon = http()
        .get(format!("http://127.0.0.1:{port}/favicon.ico"))
        .send()
        .unwrap();
    assert_eq!(favicon.status(), 404);
    assert!(fake.seen.lock().unwrap().exchanged.is_empty());

    let ok = http()
        .get(format!("{callback}?state={state}&code={CODE}"))
        .send()
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(ok.text().unwrap().contains("kfm is logged in"));
    let status = child.wait().unwrap();
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    assert!(status.success(), "{out}");
    assert!(out.contains("Logged in to"), "{out}");
    assert!(out.contains("as octomonkey"), "{out}");
    assert!(out.contains("kfm on testbox"), "{out}");
    assert_eq!(fake.seen.lock().unwrap().exchanged, [CODE]);

    let text = std::fs::read_to_string(home.config()).unwrap();
    let config: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(config["server"].as_str(), Some(fake.base.as_str()));
    assert_eq!(config["token"].as_str(), Some(TOKEN));
    assert_eq!(config["token_id"].as_str(), Some(TOKEN_ID));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.config())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // The stored token works for the next command.
    let who = home.run(&fake, &["whoami"]);
    expect(&who, 0);
    assert!(stdout(&who).contains("octomonkey"), "{}", stdout(&who));
}

#[test]
fn a_cancelled_login_stores_nothing() {
    let fake = start();
    let home = Home::new();
    let (mut child, url, mut err) = start_login(&fake, &home);
    let port = param(&url, "port").unwrap();
    let state = param(&url, "state").unwrap();
    http()
        .get(format!(
            "http://127.0.0.1:{port}/callback?state={state}&error=access_denied"
        ))
        .send()
        .unwrap();
    let status = child.wait().unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut err, &mut rest).unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(rest.contains("cancelled"), "{rest}");
    assert!(!home.config().exists());
}

#[test]
fn a_refused_code_is_the_apis_error() {
    let fake = start();
    let home = Home::new();
    let (mut child, url, mut err) = start_login(&fake, &home);
    let port = param(&url, "port").unwrap();
    let state = param(&url, "state").unwrap();
    http()
        .get(format!(
            "http://127.0.0.1:{port}/callback?state={state}&code=stale"
        ))
        .send()
        .unwrap();
    let status = child.wait().unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut err, &mut rest).unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(
        rest.contains(
            "kfm: The login code is unknown, used or expired; run kfm login again. (invalid_code)"
        ),
        "{rest}"
    );
}

#[test]
fn whoami_shows_the_user_and_teams_or_the_raw_json() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN2);
    let out = home.run(&fake, &["whoami"]);
    expect(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("twoteams"), "{text}");
    assert!(
        text.contains("Banana Squad") && text.contains("Peel Team"),
        "{text}"
    );
    assert!(text.contains("scope submit"), "{text}");

    let out = home.run(&fake, &["whoami", "--json"]);
    expect(&out, 0);
    let body: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(body["me"]["display_name"], "twoteams");
    assert_eq!(body["teams"].as_array().unwrap().len(), 2);
}

#[test]
fn without_a_token_commands_say_so() {
    let fake = start();
    let home = Home::new();
    let out = home.run(&fake, &["whoami"]);
    expect(&out, 1);
    assert!(stderr(&out).contains("not logged in"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("(unauthenticated)"),
        "{}",
        stderr(&out)
    );

    // A token stored for another server is never sent here.
    let other = Home::logged_in(&fake, TOKEN);
    let out = other
        .command(&fake, &["whoami"])
        .env("KFM_SERVER", "http://127.0.0.1:9")
        .output()
        .unwrap();
    expect(&out, 1);
    assert!(
        stderr(&out).contains("not logged in to http://127.0.0.1:9"),
        "{}",
        stderr(&out)
    );

    // KFM_TOKEN overrides the file.
    let out = home
        .command(&fake, &["whoami"])
        .env("KFM_TOKEN", TOKEN)
        .output()
        .unwrap();
    expect(&out, 0);
    assert!(stdout(&out).contains("KFM_TOKEN"), "{}", stdout(&out));
    let out = home
        .command(&fake, &["whoami"])
        .env("KFM_TOKEN", "kfm_bogus")
        .output()
        .unwrap();
    expect(&out, 1);
    assert!(stderr(&out).contains("(invalid_token)"), "{}", stderr(&out));
}

#[test]
fn logout_revokes_the_token_and_forgets_it() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let out = home.run(&fake, &["logout"]);
    expect(&out, 0);
    assert_eq!(fake.seen.lock().unwrap().revoked, [TOKEN_ID]);
    let text = std::fs::read_to_string(home.config()).unwrap();
    assert!(!text.contains(TOKEN), "{text}");
    assert!(text.contains(&fake.base), "the server stays: {text}");
    let again = home.run(&fake, &["logout"]);
    expect(&again, 0);
    assert!(stdout(&again).contains("Not logged in"));
}

#[test]
fn submit_uploads_the_sources_waits_and_activates() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let bot = bot_dir(&[
        ("bot.cpp", "int x;\n"),
        ("lib/helpers.h", "// helper\n"),
        ("out/bot.wasm", "\0asm"),
        ("out/build.log", "log"),
        (".git/HEAD", "ref"),
        (".kfm/kfm_arena", "x"),
        ("old.wasm", "x"),
    ]);
    let dir = bot.path().to_str().unwrap();
    let out = home.run(&fake, &["submit", dir, "--wait", "--activate"]);
    expect(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains(&format!("Submitted {SUB_OK} to team Banana Squad: queued")),
        "{text}"
    );
    assert!(text.contains("Ready: module sha256 abab"), "{text}");
    assert!(text.contains("Active:"), "{text}");
    assert!(stderr(&out).contains(": building"), "{}", stderr(&out));
    assert!(stderr(&out).contains("left out"), "{}", stderr(&out));

    let seen = fake.seen.lock().unwrap();
    let upload = &seen.uploads[0];
    assert_eq!(upload.team, common::TEAM_A);
    assert_eq!(upload.manifest, "language = \"cpp\"\nentry = \".\"\n");
    assert_eq!(upload.manifest_file.as_deref(), Some("manifest.toml"));
    assert_eq!(upload.zip_file.as_deref(), Some("source.zip"));
    assert_eq!(upload.zip_type.as_deref(), Some("application/zip"));
    assert_eq!(names(&upload.zip), ["bot.cpp", "lib/helpers.h"]);
    assert_eq!(zip_file(&upload.zip, "bot.cpp"), b"int x;\n");
    assert_eq!(seen.activated, [(TEAM_A.to_owned(), SUB_OK.to_owned())]);
}

#[test]
fn a_failed_build_under_wait_prints_the_log_and_exits_3() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let bot = bot_dir(&[("broken.cpp", "nope")]);
    let out = home.run(&fake, &["submit", bot.path().to_str().unwrap(), "--wait"]);
    expect(&out, 3);
    let err = stderr(&out);
    assert!(err.contains("expected unqualified-id"), "{err}");
    assert!(
        err.contains(&format!(
            "kfm: submission {SUB_FAIL} failed: the compile failed"
        )),
        "{err}"
    );
    assert!(fake.seen.lock().unwrap().activated.is_empty());
}

#[test]
fn submit_without_wait_returns_at_once_and_json_is_the_submission() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let bot = bot_dir(&[("src/bot.c", "int x;\n"), ("README.md", "# mine\n")]);
    let out = home.run(
        &fake,
        &[
            "submit",
            bot.path().to_str().unwrap(),
            "--entry",
            "./src/",
            "--json",
        ],
    );
    expect(&out, 0);
    let body: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(body["id"], SUB_OK);
    assert_eq!(body["state"], "queued");
    let seen = fake.seen.lock().unwrap();
    assert!(seen.polls.is_empty());
    assert_eq!(
        seen.uploads[0].manifest,
        "language = \"cpp\"\nentry = \"src\"\n"
    );
}

#[test]
fn several_teams_need_a_team_and_a_bad_entry_is_refused_before_upload() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN2);
    let bot = bot_dir(&[("bot.cpp", "int x;\n")]);
    let dir = bot.path().to_str().unwrap();
    let out = home.run(&fake, &["submit", dir]);
    expect(&out, 2);
    assert!(stderr(&out).contains("(ambiguous)"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Banana Squad, Peel Team"),
        "{}",
        stderr(&out)
    );

    let out = home.run(&fake, &["submit", dir, "--team", "peel team"]);
    expect(&out, 0);
    assert_eq!(fake.seen.lock().unwrap().uploads[0].team, TEAM_B);

    let out = home.run(&fake, &["submit", dir, "--team", "Nobody"]);
    expect(&out, 2);

    let out = home.run(&fake, &["submit", dir, "--team", TEAM_A, "--entry", "lib"]);
    expect(&out, 2);
    assert!(
        stderr(&out).contains("no .c, .cc or .cpp file"),
        "{}",
        stderr(&out)
    );
    let out = home.run(&fake, &["submit", dir, "--entry", "../x"]);
    expect(&out, 2);
    assert_eq!(fake.seen.lock().unwrap().uploads.len(), 1);
}

#[test]
fn bots_list_activate_and_log() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let out = home.run(&fake, &["bots", "list"]);
    expect(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("Team Banana Squad: 2 submission(s)"),
        "{text}"
    );
    assert!(text.contains(SUB_OK) && text.contains("ready"), "{text}");
    assert!(text.contains("the compile failed"), "{text}");

    let out = home.run(&fake, &["bots", "list", "--json"]);
    let body: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(body["submissions"].as_array().unwrap().len(), 2);

    let out = home.run(&fake, &["bots", "activate", SUB_OK]);
    expect(&out, 0);
    assert_eq!(
        fake.seen.lock().unwrap().activated,
        [(TEAM_A.to_owned(), SUB_OK.to_owned())]
    );

    let out = home.run(&fake, &["bots", "log", SUB_FAIL]);
    expect(&out, 0);
    assert!(stdout(&out).contains("FAILED: the compile failed"));

    let out = home.run(
        &fake,
        &["bots", "log", "0190f0f0-1c2d-7a00-8000-000000000099"],
    );
    expect(&out, 1);
    assert_eq!(stderr(&out).trim(), "kfm: Not found. (not_found)");
}

#[test]
fn matches_ladder_and_seasons_read_the_public_routes() {
    let fake = start();
    let home = Home::new();
    let out = home.run(
        &fake,
        &[
            "matches", "list", "--team", TEAM_A, "--season", "2026 12", "--page", "2",
        ],
    );
    expect(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("Banana Squad") && text.contains("Peel Team"),
        "{text}"
    );
    assert!(text.contains("1-3"), "{text}");
    assert!(text.contains("More: --page 2"), "{text}");
    assert_eq!(
        fake.seen.lock().unwrap().requests,
        [format!("/matches?page=2&season=2026 12&team={TEAM_A}")]
    );

    let out = home.run(&fake, &["matches", "show", MATCH]);
    expect(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("Winner: Peel Team"), "{text}");
    assert!(text.contains("-12.3") && text.contains("+12.3"), "{text}");
    assert!(text.contains("Fault, side A: slot 2: traps 1"), "{text}");
    assert!(text.contains("--seed 1234"), "{text}");
    let raw = home.run(&fake, &["matches", "show", MATCH, "--json"]);
    let body: Value = serde_json::from_str(&stdout(&raw)).unwrap();
    assert_eq!(body, match_summary(&fake));

    let out = home.run(&fake, &["ladder"]);
    expect(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("Season 2026-12 (current, mode contest)"),
        "{text}"
    );
    let peel = text.lines().position(|l| l.contains("Peel Team")).unwrap();
    let banana = text
        .lines()
        .position(|l| l.contains("Banana Squad"))
        .unwrap();
    assert!(peel < banana, "{text}");
    assert!(text.contains("28/3/9"), "{text}");
    let out = home.run(&fake, &["ladder", "--season", "nope"]);
    expect(&out, 1);
    assert!(stderr(&out).contains("(not_found)"));

    let out = home.run(&fake, &["seasons"]);
    expect(&out, 0);
    assert!(stdout(&out).contains("2026-12"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("engine release v0.5.1"),
        "{}",
        stdout(&out)
    );
    let raw = home.run(&fake, &["seasons", "--json"]);
    let body: Value = serde_json::from_str(&stdout(&raw)).unwrap();
    assert_eq!(body["engine_tag"], "v0.5.1");
}

#[test]
fn replay_get_downloads_without_the_bearer_and_open_prints_the_viewer() {
    let fake = start();
    let home = Home::logged_in(&fake, TOKEN);
    let dest = home.dir.path().join("replays/game.kfmr");
    let out = home.run(
        &fake,
        &["replay", "get", MATCH, "-o", dest.to_str().unwrap()],
    );
    expect(&out, 0);
    assert_eq!(std::fs::read(&dest).unwrap(), REPLAY);
    assert_eq!(fake.seen.lock().unwrap().replay_auth, [None]);

    let out = home.run(&fake, &["replay", "open", MATCH, "--no-browser"]);
    expect(&out, 0);
    assert_eq!(
        stdout(&out).trim(),
        format!("https://kfm.minip8.com/v0.5.1/?replay={}/store", fake.base)
    );

    let out = home.run(
        &fake,
        &["replay", "get", "0190f0f0-1c2d-7a00-8000-0000000000ff"],
    );
    expect(&out, 1);
    assert!(stderr(&out).contains("(not_found)"));
}

#[test]
fn usage_errors_exit_2() {
    let fake = start();
    let home = Home::new();
    expect(&home.run(&fake, &["frobnicate"]), 2);
    expect(&home.run(&fake, &["submit", "--language", "rust"]), 2);
    let out = home.run(
        &fake,
        &["test", "/no/such/module.wasm", "--arena", "/bin/true"],
    );
    expect(&out, 2);
    assert!(stderr(&out).contains("no module at"), "{}", stderr(&out));
    let out = home.run(&fake, &["build", "/no/such/dir"]);
    expect(&out, 2);
    expect(&home.run(&fake, &["--help"]), 0);
}

#[test]
fn an_unreachable_ladder_is_a_plain_error() {
    let fake = start();
    let home = Home::new();
    let out = home
        .command(&fake, &["seasons"])
        .env("KFM_SERVER", "http://127.0.0.1:9")
        .output()
        .unwrap();
    expect(&out, 1);
    assert!(
        stderr(&out).contains("cannot reach the ladder at http://127.0.0.1:9"),
        "{}",
        stderr(&out)
    );
}
