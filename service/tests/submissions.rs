//! Submissions through the API, without a worker: the upload's checks, the
//! queued row, its build job and stored source, the rate, who may see what,
//! activation, and `GET /teams/{t}`'s `active`. The uploads that get past the
//! checks need the object store (KFM_LADDER_TEST_S3_ENDPOINT; skipped without).

mod common;

use common::{
    age_submissions, bearer_json, error_code, github_browser, have_store, new_team, new_token,
    start, tiny_bot_zip, upload, zip_of, TestApp,
};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;

const MANIFEST: &str = "language = \"cpp\"\n";

/// A signed-in owner with a team and a submit token.
async fn owner(app: &TestApp, id: u64, team: &str) -> (common::Browser, String, String) {
    let mut browser = github_browser(app, id, &format!("owner{id}")).await;
    let team = new_team(&mut browser, team).await;
    let token = new_token(&mut browser, "submit").await;
    (browser, team, token)
}

async fn refused(response: reqwest::Response, status: StatusCode, code: &str) -> String {
    assert_eq!(response.status(), status);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], code, "{body}");
    body["message"].as_str().unwrap().to_owned()
}

#[sqlx::test]
async fn an_upload_is_queued_with_its_source_and_a_build_job(pool: PgPool) {
    if !have_store("an_upload_is_queued_with_its_source_and_a_build_job") {
        return;
    }
    let app = start(pool).await;
    let (_, team, token) = owner(&app, 5001, "Uploaders").await;
    let zip = zip_of(&[("bots/me/bot.cpp", b"int x;"), ("README", b"hi")]);
    let response = upload(
        &app,
        &token,
        &team,
        "language = \"c\"\nentry = \"bots/me\"\n",
        zip.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let sub: Value = response.json().await.unwrap();
    assert_eq!(sub["state"], "queued");
    assert_eq!(sub["team_id"], team.as_str());
    assert_eq!(sub["language"], "cpp");
    assert_eq!(sub["entry"], "bots/me");
    assert_eq!(sub["module_hash"], Value::Null);
    assert_eq!(sub["log_url"], Value::Null);
    assert_eq!(sub["error"], Value::Null);
    assert_eq!(sub["active"], false);
    let id = sub["id"].as_str().unwrap();

    let (kind, payload, state): (String, Value, String) =
        sqlx::query_as("SELECT kind, payload, state FROM jobs")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(kind, "build");
    assert_eq!(payload, json!({ "submission": id }));
    assert_eq!(state, "queued");
    let stored = app.store.get(&format!("sources/{id}.zip")).await.unwrap();
    assert_eq!(stored, Some(zip));

    // Listed and shown, to a read token too.
    let mut browser = github_browser(&app, 5001, "owner5001").await;
    let read = new_token(&mut browser, "read").await;
    let listed: Value = bearer_json(
        &app,
        Method::GET,
        &format!("/teams/{team}/submissions"),
        &read,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(listed["submissions"][0], sub);
    let shown: Value = bearer_json(
        &app,
        Method::GET,
        &format!("/submissions/{id}"),
        &read,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(shown, sub);
    // No log yet.
    let log = bearer_json(
        &app,
        Method::GET,
        &format!("/submissions/{id}/log"),
        &read,
        None,
    )
    .await;
    assert_eq!(log.status(), StatusCode::NOT_FOUND);

    // A team submits once a minute.
    let again = upload(&app, &token, &team, MANIFEST, tiny_bot_zip()).await;
    refused(again, StatusCode::TOO_MANY_REQUESTS, "rate_limited").await;
    age_submissions(&app.db, &team).await;
    let later = upload(&app, &token, &team, MANIFEST, tiny_bot_zip()).await;
    assert_eq!(later.status(), StatusCode::ACCEPTED);
}

#[sqlx::test]
async fn bad_uploads_are_refused_naming_the_problem(pool: PgPool) {
    let app = start(pool).await;
    let (_, team, token) = owner(&app, 5002, "Refused").await;
    for (manifest, zip, code, needle) in [
        (
            "language = \"rust\"",
            tiny_bot_zip(),
            "bad_manifest",
            "rust",
        ),
        ("lang = \"cpp\"", tiny_bot_zip(), "bad_manifest", "lang"),
        (
            "language = \"cpp\"\nentry = \"../x\"",
            tiny_bot_zip(),
            "bad_manifest",
            "..",
        ),
        (
            MANIFEST,
            zip_of(&[("../evil.c", b"x")]),
            "bad_source",
            "\"../evil.c\" contains a .. component",
        ),
        (
            MANIFEST,
            zip_of(&[("/abs.c", b"x")]),
            "bad_source",
            "absolute",
        ),
        (
            MANIFEST,
            zip_of(&[("notes.txt", b"x")]),
            "bad_source",
            "holds no .c, .cc or .cpp file",
        ),
        (
            "language = \"cpp\"\nentry = \"src\"",
            tiny_bot_zip(),
            "bad_source",
            "\"src\" holds no",
        ),
        (MANIFEST, b"not a zip".to_vec(), "bad_source", "not a zip"),
    ] {
        let response = upload(&app, &token, &team, manifest, zip).await;
        let message = refused(response, StatusCode::BAD_REQUEST, code).await;
        assert!(message.contains(needle), "{manifest}: {message}");
    }
    // Over 4 MiB of source.
    let big = vec![7u8; 4 * 1024 * 1024 + 1];
    let response = upload(&app, &token, &team, MANIFEST, big).await;
    refused(response, StatusCode::PAYLOAD_TOO_LARGE, "too_large").await;

    // A part missing.
    let form = reqwest::multipart::Form::new().text("manifest.toml", MANIFEST);
    let response = reqwest::Client::new()
        .post(format!("{}/teams/{team}/submissions", app.base))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    let message = refused(response, StatusCode::BAD_REQUEST, "bad_source").await;
    assert!(message.contains("source.zip is missing"), "{message}");
    // Not multipart at all.
    let response = bearer_json(
        &app,
        Method::POST,
        &format!("/teams/{team}/submissions"),
        &token,
        Some(json!({})),
    )
    .await;
    refused(response, StatusCode::BAD_REQUEST, "bad_request").await;

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM submissions")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "a refused upload leaves nothing");
}

#[sqlx::test]
async fn only_members_with_the_right_credentials_submit(pool: PgPool) {
    let app = start(pool).await;
    let (mut browser, team, _) = owner(&app, 5003, "Guarded").await;
    let read = new_token(&mut browser, "read").await;
    let response = upload(&app, &read, &team, MANIFEST, tiny_bot_zip()).await;
    refused(response, StatusCode::FORBIDDEN, "insufficient_scope").await;

    let mut stranger = github_browser(&app, 5004, "stranger").await;
    let theirs = new_token(&mut stranger, "submit").await;
    let response = upload(&app, &theirs, &team, MANIFEST, tiny_bot_zip()).await;
    refused(response, StatusCode::FORBIDDEN, "forbidden").await;
    let response = upload(
        &app,
        &theirs,
        "0190f0f0-1c2d-7a00-8000-0123456789ab",
        MANIFEST,
        tiny_bot_zip(),
    )
    .await;
    refused(response, StatusCode::NOT_FOUND, "not_found").await;
    let response = bearer_json(
        &app,
        Method::GET,
        &format!("/teams/{team}/submissions"),
        &theirs,
        None,
    )
    .await;
    refused(response, StatusCode::FORBIDDEN, "forbidden").await;

    // A session upload needs its CSRF header.
    let form = reqwest::multipart::Form::new()
        .text("manifest.toml", MANIFEST)
        .part(
            "source.zip",
            reqwest::multipart::Part::bytes(tiny_bot_zip()),
        );
    let request = browser
        .http
        .post(format!("{}/teams/{team}/submissions", app.base))
        .multipart(form);
    let response = browser.send(request).await;
    assert_eq!(error_code(response).await, "bad_csrf");
}

#[sqlx::test]
async fn a_session_with_its_csrf_header_submits(pool: PgPool) {
    if !have_store("a_session_with_its_csrf_header_submits") {
        return;
    }
    let app = start(pool).await;
    let (mut browser, team, _) = owner(&app, 5005, "Sessioned").await;
    let csrf = browser.csrf().await;
    let form = reqwest::multipart::Form::new()
        .text("manifest.toml", MANIFEST)
        .part(
            "source.zip",
            reqwest::multipart::Part::bytes(tiny_bot_zip()),
        );
    let request = browser
        .http
        .post(format!("{}/teams/{team}/submissions", app.base))
        .header("x-csrf-token", csrf)
        .multipart(form);
    let response = browser.send(request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// Inserts a submission of `team` in `state` directly; its id.
async fn insert_submission(db: &PgPool, team: &str, state: &str) -> String {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO submissions (id, team_id, manifest, source_key, state) \
         VALUES ($1, $2::uuid, 'language = \"cpp\"', $3, $4)",
    )
    .bind(id)
    .bind(team)
    .bind(format!("sources/{id}.zip"))
    .bind(state)
    .execute(db)
    .await
    .unwrap();
    id.to_string()
}

#[sqlx::test]
async fn only_a_ready_submission_of_the_team_is_activated(pool: PgPool) {
    let app = start(pool).await;
    let (mut browser, team, token) = owner(&app, 5006, "Activators").await;
    let ready = insert_submission(&app.db, &team, "ready").await;
    let queued = insert_submission(&app.db, &team, "queued").await;
    let (_, other_team, other_token) = owner(&app, 5007, "Others").await;
    let theirs = insert_submission(&app.db, &other_team, "ready").await;

    let shown: Value = reqwest::get(format!("{}/teams/{team}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["active"], Value::Null);

    let activate = |secret: String, team: String, sub: String| {
        let app_base = app.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{app_base}/teams/{team}/active"))
                .bearer_auth(secret)
                .json(&json!({ "submission": sub }))
                .send()
                .await
                .unwrap()
        }
    };
    let response = activate(token.clone(), team.clone(), queued.clone()).await;
    refused(response, StatusCode::CONFLICT, "not_ready").await;
    let response = activate(token.clone(), team.clone(), theirs.clone()).await;
    refused(response, StatusCode::NOT_FOUND, "not_found").await;
    let response = activate(other_token.clone(), team.clone(), ready.clone()).await;
    refused(response, StatusCode::FORBIDDEN, "forbidden").await;
    let read = new_token(&mut browser, "read").await;
    let response = activate(read, team.clone(), ready.clone()).await;
    refused(response, StatusCode::FORBIDDEN, "insufficient_scope").await;

    let response = activate(token.clone(), team.clone(), ready.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let sub: Value = response.json().await.unwrap();
    assert_eq!(sub["id"], ready.as_str());
    assert_eq!(sub["active"], true);
    let shown: Value = reqwest::get(format!("{}/teams/{team}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["active"], ready.as_str());

    // A member (not an owner) with a session activates too; the CSRF header is required.
    let mut member = github_browser(&app, 5008, "member5008").await;
    let member_id = member.user_id().await;
    let csrf = browser.csrf().await;
    let added = browser
        .json(
            Method::POST,
            &format!("/teams/{team}/members"),
            Some(json!({ "user": member_id })),
            Some(&csrf),
        )
        .await;
    assert_eq!(added.status(), StatusCode::OK);
    let second = insert_submission(&app.db, &team, "ready").await;
    let path = format!("/teams/{team}/active");
    let response = member
        .json(
            Method::POST,
            &path,
            Some(json!({ "submission": second })),
            None,
        )
        .await;
    assert_eq!(error_code(response).await, "bad_csrf");
    let csrf = member.csrf().await;
    let response = member
        .json(
            Method::POST,
            &path,
            Some(json!({ "submission": second })),
            Some(&csrf),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let shown: Value = reqwest::get(format!("{}/teams/{team}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["active"], second.as_str());
}

#[sqlx::test]
async fn a_submission_is_visible_to_its_team_only(pool: PgPool) {
    let app = start(pool).await;
    let (_, team, token) = owner(&app, 5009, "Private").await;
    let id = insert_submission(&app.db, &team, "failed").await;
    let response = bearer_json(
        &app,
        Method::GET,
        &format!("/submissions/{id}"),
        &token,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let sub: Value = response.json().await.unwrap();
    assert_eq!(sub["state"], "failed");

    let mut stranger = github_browser(&app, 5010, "peeker").await;
    let theirs = new_token(&mut stranger, "read").await;
    for path in [
        format!("/submissions/{id}"),
        format!("/submissions/{id}/log"),
    ] {
        let response = bearer_json(&app, Method::GET, &path, &theirs, None).await;
        refused(response, StatusCode::NOT_FOUND, "not_found").await;
    }
    let response = reqwest::get(format!("{}/submissions/{id}", app.base))
        .await
        .unwrap();
    refused(response, StatusCode::UNAUTHORIZED, "unauthenticated").await;
}
