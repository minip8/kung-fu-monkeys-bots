//! API tokens: create, list, revoke, bearer authentication and scopes.

mod common;

use axum::routing::post;
use axum::Router;
use common::{error_code, github_browser, start, start_with};
use kfm_ladder::auth::Caller;
use kfm_ladder::error::AppResult;
use kfm_ladder::tokens::Scope;
use kfm_ladder::AppState;
use reqwest::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;

async fn create(
    browser: &mut common::Browser,
    csrf: &str,
    name: &str,
    scope: &str,
) -> serde_json::Value {
    let response = browser
        .json(
            Method::POST,
            "/tokens",
            Some(json!({ "name": name, "scope": scope })),
            Some(csrf),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    response.json().await.unwrap()
}

fn bearer(
    app: &common::TestApp,
    method: Method,
    path: &str,
    secret: &str,
) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .bearer_auth(secret)
}

#[sqlx::test]
async fn a_token_is_shown_once_and_stored_as_its_hash(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = github_browser(&app, 2001, "tokener").await;
    let csrf = browser.csrf().await;
    let created = create(&mut browser, &csrf, " laptop ", "submit").await;
    let secret = created["secret"].as_str().unwrap();
    assert!(secret.starts_with("kfm_") && secret.len() == 47, "{secret}");
    assert_eq!(created["name"], "laptop");
    assert_eq!(created["scope"], "submit");
    assert!(created["created_at"].is_string());
    assert!(created["last_used_at"].is_null());

    let (hash,): (Vec<u8>,) = sqlx::query_as("SELECT hash FROM tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    use sha2::Digest;
    assert_eq!(hash, sha2::Sha256::digest(secret.as_bytes()).to_vec());

    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    let tokens = listed["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0]["id"], created["id"]);
    assert!(tokens[0].get("secret").is_none());
    assert!(!listed.to_string().contains(secret));
}

#[sqlx::test]
async fn a_bearer_token_authenticates_and_records_its_use(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2002, "bearer").await;
    let csrf = browser.csrf().await;
    let user_id = browser.user_id().await;
    let created = create(&mut browser, &csrf, "cli", "read").await;
    let secret = created["secret"].as_str().unwrap();

    let me: serde_json::Value = bearer(&app, Method::GET, "/me", secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["id"], user_id.as_str());
    assert_eq!(me["auth"], "token");
    assert_eq!(me["scope"], "read");
    assert!(me.get("csrf").is_none());

    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert!(listed["tokens"][0]["last_used_at"].is_string());
}

#[sqlx::test]
async fn a_revoked_token_is_refused(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2003, "revoker").await;
    let csrf = browser.csrf().await;
    let created = create(&mut browser, &csrf, "old", "submit").await;
    let secret = created["secret"].as_str().unwrap().to_owned();
    let id = created["id"].as_str().unwrap();

    let without_csrf = browser
        .json(Method::DELETE, &format!("/tokens/{id}"), None, None)
        .await;
    assert_eq!(error_code(without_csrf).await, "bad_csrf");
    let revoked = browser
        .json(Method::DELETE, &format!("/tokens/{id}"), None, Some(&csrf))
        .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    let again = browser
        .json(Method::DELETE, &format!("/tokens/{id}"), None, Some(&csrf))
        .await;
    assert_eq!(error_code(again).await, "not_found");

    let response = bearer(&app, Method::GET, "/me", &secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(response).await, "invalid_token");
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert!(listed["tokens"].as_array().unwrap().is_empty());
}

#[sqlx::test]
async fn unknown_and_malformed_bearers_are_invalid_even_with_a_session(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2004, "mixed").await;
    for bad in [
        "kfm_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "nope",
        "",
    ] {
        let request = browser
            .http
            .get(format!("{}/me", app.base))
            .bearer_auth(bad);
        let response = browser.send(request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{bad}");
        assert_eq!(error_code(response).await, "invalid_token");
    }
}

#[sqlx::test]
async fn token_management_needs_the_portal_session_but_a_token_may_revoke_itself(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2005, "manager").await;
    let csrf = browser.csrf().await;
    let created = create(&mut browser, &csrf, "cli", "submit").await;
    let secret = created["secret"].as_str().unwrap();
    let id = created["id"].as_str().unwrap();

    let list = bearer(&app, Method::GET, "/tokens", secret)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(list).await, "session_required");
    let make = bearer(&app, Method::POST, "/tokens", secret)
        .json(&json!({ "name": "more", "scope": "submit" }))
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(make).await, "session_required");

    // A token revokes only itself (v1.3, `kfm logout`): another one needs the session.
    let other = create(&mut browser, &csrf, "other", "read").await;
    let other_id = other["id"].as_str().unwrap();
    let revoke = bearer(&app, Method::DELETE, &format!("/tokens/{other_id}"), secret)
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(revoke).await, "session_required");
    let by_other = bearer(
        &app,
        Method::DELETE,
        &format!("/tokens/{id}"),
        other["secret"].as_str().unwrap(),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(error_code(by_other).await, "session_required");
    let itself = bearer(&app, Method::DELETE, &format!("/tokens/{id}"), secret)
        .send()
        .await
        .unwrap();
    assert_eq!(itself.status(), StatusCode::NO_CONTENT);
    let after = bearer(&app, Method::GET, "/me", secret)
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(after).await, "invalid_token");
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert_eq!(listed["tokens"].as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn bad_requests_are_named(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2006, "careless").await;
    let csrf = browser.csrf().await;
    let cases = [
        (json!({ "name": "x", "scope": "admin" }), "invalid_scope"),
        (json!({ "name": "  ", "scope": "read" }), "invalid_name"),
        (
            json!({ "name": "x".repeat(65), "scope": "read" }),
            "invalid_name",
        ),
        (json!({ "scope": "read" }), "bad_request"),
    ];
    for (body, code) in cases {
        let response = browser
            .json(Method::POST, "/tokens", Some(body.clone()), Some(&csrf))
            .await;
        assert_eq!(error_code(response).await, code, "{body}");
    }
    let response = browser
        .send(
            browser
                .http
                .post(format!("{}/tokens", app.base))
                .header("x-csrf-token", &csrf)
                .body("name=x&scope=read"),
        )
        .await;
    assert_eq!(error_code(response).await, "bad_request");
}

#[sqlx::test]
async fn live_tokens_are_capped_per_user(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 2007, "hoarder").await;
    let csrf = browser.csrf().await;
    for i in 0..kfm_ladder::tokens::MAX_LIVE_TOKENS {
        create(&mut browser, &csrf, &format!("t{i}"), "read").await;
    }
    let response = browser
        .json(
            Method::POST,
            "/tokens",
            Some(json!({ "name": "one more", "scope": "read" })),
            Some(&csrf),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "limit_reached");
}

#[sqlx::test]
async fn another_users_token_cannot_be_revoked(pool: PgPool) {
    let app = start(pool).await;
    let mut alice = github_browser(&app, 2008, "alice").await;
    let mut bob = github_browser(&app, 2009, "bob").await;
    let alice_csrf = alice.csrf().await;
    let bob_csrf = bob.csrf().await;
    let created = create(&mut alice, &alice_csrf, "mine", "read").await;
    let id = created["id"].as_str().unwrap();
    let response = bob
        .json(
            Method::DELETE,
            &format!("/tokens/{id}"),
            None,
            Some(&bob_csrf),
        )
        .await;
    assert_eq!(error_code(response).await, "not_found");
    let listed: serde_json::Value = alice.get("/tokens").await.json().await.unwrap();
    assert_eq!(listed["tokens"].as_array().unwrap().len(), 1);
}

/// A route that needs `submit`, as step 3b's upload will.
async fn needs_submit(caller: Caller) -> AppResult<&'static str> {
    caller.require_scope(Scope::Submit)?;
    Ok("ok")
}

#[sqlx::test]
async fn scopes_gate_what_a_token_may_do(pool: PgPool) {
    let probe: Router<AppState> = Router::new().route("/probe/submit", post(needs_submit));
    let app = start_with(pool, probe).await;
    let mut browser = github_browser(&app, 2010, "scoper").await;
    let csrf = browser.csrf().await;
    let read = create(&mut browser, &csrf, "read", "read").await;
    let submit = create(&mut browser, &csrf, "submit", "submit").await;

    let denied = bearer(
        &app,
        Method::POST,
        "/probe/submit",
        read["secret"].as_str().unwrap(),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(denied).await, "insufficient_scope");
    let allowed = bearer(
        &app,
        Method::POST,
        "/probe/submit",
        submit["secret"].as_str().unwrap(),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    // A session may do anything its account may.
    let by_session = browser
        .json(Method::POST, "/probe/submit", None, None)
        .await;
    assert_eq!(by_session.status(), StatusCode::OK);
    // Both scopes read.
    for token in [&read, &submit] {
        let teams = bearer(
            &app,
            Method::GET,
            "/teams",
            token["secret"].as_str().unwrap(),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(teams.status(), StatusCode::OK);
    }
}
