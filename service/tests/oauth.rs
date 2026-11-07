//! Sign-in with GitHub and Google against the in-process fake providers:
//! account creation, sign-in again, linking, the state cookie, sessions.

mod common;

use common::{error_code, location, start, Browser, FakeUser, Provider};
use reqwest::{Method, StatusCode};
use sqlx::PgPool;

async fn count(pool: &PgPool, table: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

#[sqlx::test]
async fn github_sign_in_creates_an_account_keyed_by_the_numeric_id(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    assert_eq!(
        error_code(browser.get("/me").await).await,
        "unauthenticated"
    );

    let landed = browser
        .oauth(&app, Provider::GitHub, FakeUser::new("1001", "octomonkey"))
        .await;
    assert_eq!(landed, "/");
    assert!(browser.cookies.contains_key("kfm_session"));
    assert!(
        !browser.cookies.contains_key("kfm_oauth"),
        "state cookie cleared"
    );

    let me = browser.me().await;
    assert_eq!(me["display_name"], "octomonkey");
    assert_eq!(me["auth"], "session");
    assert!(me["csrf"].is_string());
    assert!(me.get("scope").is_none());
    let identities = me["identities"].as_array().unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0]["provider"], "github");
    assert_eq!(identities[0]["name"], "octomonkey");
    assert_eq!(identities[0]["email"], "octomonkey@example.org");

    let (subject,): (String,) = sqlx::query_as("SELECT subject FROM identities")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subject, "1001");
    assert!(app.fake.seen().contains(&"github/token".to_owned()));
    assert!(!app.fake.seen().contains(&"github/user/emails".to_owned()));
}

#[sqlx::test]
async fn a_hidden_github_email_is_read_from_the_emails_list(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    let mut user = FakeUser::new("1002", "Shy");
    user.hide_email = true;
    browser.sign_in(&app, Provider::GitHub, user).await;
    let me = browser.me().await;
    assert_eq!(me["identities"][0]["email"], "shy@example.org");
    assert!(app.fake.seen().contains(&"github/user/emails".to_owned()));
}

#[sqlx::test]
async fn google_sign_in_verifies_the_id_token(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(
            &app,
            Provider::Google,
            FakeUser::new("g-sub-77", "Ada Lovelace"),
        )
        .await;
    let me = browser.me().await;
    assert_eq!(me["display_name"], "Ada Lovelace");
    assert_eq!(me["identities"][0]["provider"], "google");
    assert_eq!(me["identities"][0]["email"], "ada.lovelace@example.org");
    let (subject,): (String,) = sqlx::query_as("SELECT subject FROM identities")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subject, "g-sub-77");
}

#[sqlx::test]
async fn an_id_token_with_the_wrong_nonce_is_refused(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    app.fake.set_wrong_nonce(true);
    let landed = browser
        .oauth(&app, Provider::Google, FakeUser::new("g-1", "Eve"))
        .await;
    assert_eq!(landed, "/?error=oauth_failed");
    assert!(!browser.cookies.contains_key("kfm_session"));
    assert_eq!(count(&pool, "users").await, 0);
}

#[sqlx::test]
async fn signing_in_again_finds_the_same_account(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut first = Browser::new(&app);
    first
        .sign_in(&app, Provider::GitHub, FakeUser::new("1003", "repeat"))
        .await;
    let id = first.user_id().await;

    let mut second = Browser::new(&app);
    let mut renamed = FakeUser::new("1003", "repeat-renamed");
    renamed.email = Some("new@example.org".to_owned());
    second.sign_in(&app, Provider::GitHub, renamed).await;
    let me = second.me().await;
    assert_eq!(me["id"], id.as_str());
    // The identity's display data follows the provider; the account keeps its name.
    assert_eq!(me["display_name"], "repeat");
    assert_eq!(me["identities"][0]["name"], "repeat-renamed");
    assert_eq!(me["identities"][0]["email"], "new@example.org");
    assert_eq!(count(&pool, "users").await, 1);
    assert_eq!(count(&pool, "sessions").await, 2, "one session per browser");
}

#[sqlx::test]
async fn the_same_email_from_two_providers_is_two_accounts(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut a = Browser::new(&app);
    a.sign_in(&app, Provider::GitHub, FakeUser::new("1004", "same"))
        .await;
    let mut b = Browser::new(&app);
    b.sign_in(&app, Provider::Google, FakeUser::new("g-same", "same"))
        .await;
    assert_ne!(a.user_id().await, b.user_id().await);
    assert_eq!(count(&pool, "users").await, 2);
}

#[sqlx::test]
async fn a_signed_in_user_links_the_other_provider(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::GitHub, FakeUser::new("1005", "linker"))
        .await;
    let id = browser.user_id().await;
    let session = browser.cookies["kfm_session"].clone();

    let landed = browser
        .oauth(&app, Provider::Google, FakeUser::new("g-link", "Linker G"))
        .await;
    assert_eq!(landed, "/?linked=google");
    assert_eq!(
        browser.cookies["kfm_session"], session,
        "linking keeps the session"
    );
    let me = browser.me().await;
    assert_eq!(me["id"], id.as_str());
    let providers: Vec<&str> = me["identities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["provider"].as_str().unwrap())
        .collect();
    assert_eq!(providers, ["github", "google"]);

    // Signing in with Google alone now reaches the same account.
    let mut elsewhere = Browser::new(&app);
    elsewhere
        .sign_in(&app, Provider::Google, FakeUser::new("g-link", "Linker G"))
        .await;
    assert_eq!(elsewhere.user_id().await, id);
    assert_eq!(count(&pool, "users").await, 1);
}

#[sqlx::test]
async fn a_second_identity_of_the_same_provider_is_refused(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::GitHub, FakeUser::new("1006", "one"))
        .await;
    let landed = browser
        .oauth(&app, Provider::GitHub, FakeUser::new("1007", "two"))
        .await;
    assert_eq!(landed, "/?error=already_linked");
    assert_eq!(count(&pool, "identities").await, 1);
    assert_eq!(browser.me().await["display_name"], "one", "still signed in");
}

#[sqlx::test]
async fn an_identity_of_another_account_signs_in_as_that_account(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = Browser::new(&app);
    owner
        .sign_in(&app, Provider::Google, FakeUser::new("g-owner", "Owner"))
        .await;
    let owner_id = owner.user_id().await;

    let mut other = Browser::new(&app);
    other
        .sign_in(&app, Provider::GitHub, FakeUser::new("1008", "other"))
        .await;
    let old_session = other.cookies["kfm_session"].clone();
    let landed = other
        .oauth(&app, Provider::Google, FakeUser::new("g-owner", "Owner"))
        .await;
    assert_eq!(landed, "/");
    assert_ne!(
        other.cookies["kfm_session"], old_session,
        "a new session for the other account"
    );
    assert_eq!(other.user_id().await, owner_id);
}

#[sqlx::test]
async fn a_callback_without_the_state_cookie_is_refused(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    app.fake
        .set_next(Provider::GitHub, FakeUser::new("1009", "csrf"));
    let start = browser.get("/auth/github").await;
    let at_provider = browser.get(&location(&start)).await;
    let callback = location(&at_provider);

    // Another browser (no state cookie) presents the code.
    let mut attacker = Browser::new(&app);
    let done = attacker.get(&callback).await;
    assert_eq!(location(&done), "/?error=oauth_state");

    // A tampered state parameter fails too.
    let tampered = callback.replace("state=", "state=x");
    let done = browser.get(&tampered).await;
    assert_eq!(location(&done), "/?error=oauth_state");
    assert_eq!(count(&pool, "users").await, 0);
}

#[sqlx::test]
async fn a_state_cookie_of_the_other_provider_is_refused(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    app.fake
        .set_next(Provider::GitHub, FakeUser::new("1010", "mixed"));
    let start = browser.get("/auth/github").await;
    let at_provider = browser.get(&location(&start)).await;
    let callback = location(&at_provider).replace("/auth/github/", "/auth/google/");
    let done = browser.get(&callback).await;
    assert_eq!(location(&done), "/?error=oauth_state");
}

#[sqlx::test]
async fn a_denied_consent_lands_on_the_portal(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    app.fake.set_deny(true);
    let landed = browser
        .oauth(&app, Provider::GitHub, FakeUser::new("1011", "no"))
        .await;
    assert_eq!(landed, "/?error=oauth_denied");
}

#[sqlx::test]
async fn logout_needs_the_csrf_token_and_ends_the_session(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::GitHub, FakeUser::new("1012", "leaver"))
        .await;
    let csrf = browser.csrf().await;

    let refused = browser
        .post_form("/auth/logout", &[("csrf", "wrong")])
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(refused).await, "bad_csrf");
    let refused = browser.json(Method::POST, "/auth/logout", None, None).await;
    assert_eq!(error_code(refused).await, "bad_csrf");

    let stolen = browser.cookies.clone();
    let done = browser.post_form("/auth/logout", &[("csrf", &csrf)]).await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert!(!browser.cookies.contains_key("kfm_session"));
    assert_eq!(count(&pool, "sessions").await, 0);

    // The old cookie no longer works.
    browser.cookies = stolen;
    assert_eq!(
        error_code(browser.get("/me").await).await,
        "unauthenticated"
    );
}

#[sqlx::test]
async fn logout_by_header_works_for_json_clients(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::Google, FakeUser::new("g-2", "Json"))
        .await;
    let csrf = browser.csrf().await;
    let done = browser
        .json(Method::POST, "/auth/logout", None, Some(&csrf))
        .await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert_eq!(count(&pool, "sessions").await, 0);
}

#[sqlx::test]
async fn sessions_expire_after_thirty_idle_days(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::GitHub, FakeUser::new("1013", "idle"))
        .await;
    sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '29 days'")
        .execute(&pool)
        .await
        .unwrap();
    browser.me().await; // still alive, and touched
    let (fresh,): (bool,) =
        sqlx::query_as("SELECT last_seen_at > now() - interval '1 minute' FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(fresh, "a request records itself");
    sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '31 days'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        error_code(browser.get("/me").await).await,
        "unauthenticated"
    );
}

#[sqlx::test]
async fn a_forged_session_cookie_is_ignored(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(&app, Provider::GitHub, FakeUser::new("1014", "forger"))
        .await;
    let real = browser.cookies["kfm_session"].clone();
    let (id, _) = real.split_once('.').unwrap();
    browser
        .cookies
        .insert("kfm_session".to_owned(), format!("{id}.AAAA"));
    assert_eq!(
        error_code(browser.get("/me").await).await,
        "unauthenticated"
    );
}

#[sqlx::test]
async fn cookies_are_http_only_and_lax(pool: PgPool) {
    let app = start(pool).await;
    let browser = Browser::new(&app);
    app.fake
        .set_next(Provider::GitHub, FakeUser::new("1015", "flags"));
    let start = browser
        .http
        .get(format!("{}/auth/github", app.base))
        .send()
        .await
        .unwrap();
    let cookie = start.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("kfm_oauth="), "{cookie}");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    // The test origin is plain http, so no Secure (an https public URL adds it).
    assert!(!cookie.contains("Secure"));
}
