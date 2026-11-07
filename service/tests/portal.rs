//! The portal: the signed-out and signed-in pages and the form handlers with
//! their CSRF check.

mod common;

use common::{github_browser, location, start, Browser, FakeUser, Provider};
use reqwest::StatusCode;
use sqlx::PgPool;

async fn page(browser: &mut Browser, path: &str) -> String {
    let response = browser.get(path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    response.text().await.unwrap()
}

/// The page's CSRF token, as a form would carry it.
fn csrf_in(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("a form with a csrf field") + marker.len();
    html[start..start + html[start..].find('"').unwrap()].to_owned()
}

#[sqlx::test]
async fn signed_out_shows_both_providers(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    let response = browser.get("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["cache-control"], "no-store");
    assert!(headers["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'none'"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let html = response.text().await.unwrap();
    assert!(html.contains("href=\"/auth/github\""));
    assert!(html.contains("href=\"/auth/google\""));
    assert!(!html.contains("<script"));
    assert!(!html.contains("name=\"csrf\""));
}

#[sqlx::test]
async fn signed_in_shows_the_account_and_a_link_button(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4001, "portal-user").await;
    let html = page(&mut browser, "/").await;
    assert!(html.contains("Signed in as portal-user"));
    assert!(html.contains("portal-user@example.org"));
    assert!(html.contains("href=\"/auth/google\">Link Google"));
    assert!(!html.contains("Link GitHub"));
    assert_eq!(csrf_in(&html), browser.csrf().await);

    let landed = browser
        .oauth(&app, Provider::Google, FakeUser::new("g-p", "Portal G"))
        .await;
    let html = page(&mut browser, &landed).await;
    assert!(html.contains("Google is now linked"));
    assert!(!html.contains("Link Google"));
}

#[sqlx::test]
async fn an_error_code_is_shown_as_its_message(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    let html = page(&mut browser, "/?error=oauth_state").await;
    assert!(html.contains("The sign-in expired"));
    let html = page(&mut browser, "/?error=<script>").await;
    assert!(!html.contains("<script>"));
}

#[sqlx::test]
async fn a_token_form_shows_the_secret_once(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4002, "former").await;
    let csrf = csrf_in(&page(&mut browser, "/").await);
    let response = browser
        .post_form(
            "/portal/tokens",
            &[("csrf", &csrf), ("name", "desk"), ("scope", "submit")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    let start = html.find("kfm_").unwrap();
    let secret = &html[start..start + 47];
    assert!(html.contains("it is not shown again"));

    let reloaded = page(&mut browser, "/").await;
    assert!(reloaded.contains("desk"));
    assert!(!reloaded.contains(secret));

    // The secret works as a bearer.
    let me = reqwest::Client::new()
        .get(format!("{}/me", app.base))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);

    // Revoke it through its form.
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    let id = listed["tokens"][0]["id"].as_str().unwrap();
    let response = browser
        .post_form(&format!("/portal/tokens/{id}/revoke"), &[("csrf", &csrf)])
        .await;
    assert_eq!(location(&response), "/");
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert!(listed["tokens"].as_array().unwrap().is_empty());
}

#[sqlx::test]
async fn a_form_without_the_csrf_token_changes_nothing(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = github_browser(&app, 4003, "victim").await;
    for (path, form) in [
        (
            "/portal/tokens",
            vec![("name", "evil"), ("scope", "submit")],
        ),
        ("/portal/teams", vec![("name", "Evil Team")]),
    ] {
        let mut with_wrong = form.clone();
        with_wrong.push(("csrf", "forged"));
        for fields in [form, with_wrong] {
            let response = browser.post_form(path, &fields).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(location(&response), "/?error=bad_csrf", "{path}");
        }
    }
    let (tokens,): (i64,) = sqlx::query_as("SELECT count(*) FROM tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (teams,): (i64,) = sqlx::query_as("SELECT count(*) FROM teams")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((tokens, teams), (0, 0));
    // Signed out, a form asks to sign in.
    let mut anonymous = Browser::new(&app);
    let response = anonymous
        .post_form("/portal/teams", &[("csrf", "x"), ("name", "Ghosts")])
        .await;
    assert_eq!(location(&response), "/?error=unauthenticated");
}

#[sqlx::test]
async fn team_forms_create_add_and_leave(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 4004, "skipper").await;
    let mut mate = github_browser(&app, 4005, "mate").await;
    let csrf = csrf_in(&page(&mut owner, "/").await);

    let response = owner
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "Form Team")])
        .await;
    assert_eq!(location(&response), "/");
    let response = owner
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "x")])
        .await;
    assert_eq!(location(&response), "/?error=invalid_name");
    let html = page(&mut owner, "/?error=invalid_name").await;
    assert!(html.contains("Form Team"));
    assert!(html.contains("naming rules"));

    let teams: serde_json::Value = owner.get("/teams").await.json().await.unwrap();
    let team = teams["teams"][0]["id"].as_str().unwrap().to_owned();
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/members"),
            &[("csrf", &csrf), ("user", "mate")],
        )
        .await;
    assert_eq!(location(&response), "/");

    let mate_html = page(&mut mate, "/").await;
    assert!(mate_html.contains("Form Team"));
    assert!(mate_html.contains(">Leave<"));
    let mate_id = mate.user_id().await;
    let mate_csrf = csrf_in(&mate_html);
    let response = mate
        .post_form(
            &format!("/portal/teams/{team}/members/{mate_id}/remove"),
            &[("csrf", &mate_csrf)],
        )
        .await;
    assert_eq!(location(&response), "/");
    assert!(!page(&mut mate, "/").await.contains("Form Team"));

    let owner_id = owner.user_id().await;
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/members/{owner_id}/remove"),
            &[("csrf", &csrf)],
        )
        .await;
    assert_eq!(location(&response), "/?error=last_owner");
}

#[sqlx::test]
async fn names_are_escaped(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    browser
        .sign_in(
            &app,
            Provider::Google,
            FakeUser::new("g-x", "<img src=x onerror=alert(1)>"),
        )
        .await;
    let html = page(&mut browser, "/").await;
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("&#60;img") || html.contains("&lt;img"));
}

#[sqlx::test]
async fn the_portal_submits_lists_and_activates(pool: PgPool) {
    if !common::have_store("the_portal_submits_lists_and_activates") {
        return;
    }
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4010, "portal-submitter").await;
    let team = common::new_team(&mut browser, "Portal Team").await;
    let html = page(&mut browser, "/").await;
    assert!(html.contains(&format!(
        "action=\"/portal/teams/{team}/submissions\" enctype=\"multipart/form-data\""
    )));
    assert!(html.contains("No submissions yet."));
    let csrf = csrf_in(&html);

    let form = |csrf: &str, zip: Vec<u8>| {
        reqwest::multipart::Form::new()
            .text("csrf", csrf.to_owned())
            .text("language", "c")
            .text("entry", "")
            .part(
                "source.zip",
                reqwest::multipart::Part::bytes(zip).file_name("bot.zip"),
            )
    };
    let path = format!("{}/portal/teams/{team}/submissions", app.base);

    // A wrong CSRF token changes nothing.
    let request = browser
        .http
        .post(&path)
        .multipart(form("nope", common::tiny_bot_zip()));
    let response = browser.send(request).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/?error=bad_csrf");

    // A refused archive comes back with what was wrong.
    let bad = common::zip_of(&[("../x.c", b"x")]);
    let request = browser.http.post(&path).multipart(form(&csrf, bad));
    let response = browser.send(request).await;
    let landed = location(&response);
    assert!(landed.starts_with("/?error=bad_source&detail="), "{landed}");
    let html = page(&mut browser, &landed).await;
    assert!(
        html.contains("The source archive is refused: &#34;../x.c&#34; contains a .. component."),
        "{html}"
    );
    // A detail is shown only for the codes that carry one.
    let html = page(&mut browser, "/?error=forbidden&detail=injected").await;
    assert!(!html.contains("injected"));

    let request = browser
        .http
        .post(&path)
        .multipart(form(&csrf, common::tiny_bot_zip()));
    let response = browser.send(request).await;
    assert_eq!(location(&response), "/");
    let (id, state): (uuid::Uuid, String) = sqlx::query_as("SELECT id, state FROM submissions")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(state, "queued");
    let html = page(&mut browser, "/").await;
    assert!(html.contains("queued"));
    assert!(
        !html.contains(&format!("value=\"{id}\"")),
        "no activate button while queued"
    );

    // Ready (as a worker would leave it): a log link and an activate button.
    sqlx::query(
        "UPDATE submissions SET state = 'ready', log_key = 'logs/x.txt', \
         module_hash = decode(repeat('ab', 32), 'hex')",
    )
    .execute(&app.db)
    .await
    .unwrap();
    let html = page(&mut browser, "/").await;
    assert!(html.contains(&format!("href=\"/submissions/{id}/log\"")));
    assert!(html.contains("<code title=\"SHA-256 of the module\">abababababab</code>"));
    assert!(html.contains(&format!("name=\"submission\" value=\"{id}\"")));
    let response = browser
        .post_form(
            &format!("/portal/teams/{team}/active"),
            &[("csrf", &csrf), ("submission", &id.to_string())],
        )
        .await;
    assert_eq!(location(&response), "/");
    let html = page(&mut browser, "/").await;
    assert!(html.contains("<strong class=\"good\">active</strong>"));
    assert!(!html.contains(&format!("name=\"submission\" value=\"{id}\"")));
}

#[sqlx::test]
async fn the_privacy_page_is_public_and_linked_from_every_page(pool: PgPool) {
    let app = common::start(pool).await;
    let response = reqwest::get(format!("{}/privacy", app.base)).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let html = response.text().await.unwrap();
    assert!(html.contains("<h1>Privacy</h1>"), "{html}");
    assert!(html.contains(kfm_ladder::portal::PRIVACY_DATE));
    assert!(html.contains("24 hours"));
    // The default contact is the repository's issues page, as a link.
    assert!(html.contains(&format!(
        "<a href=\"{0}\">{0}</a>",
        kfm_ladder::config::DEFAULT_CONTACT
    )));
    // The footer of the shared layout links to it, signed out.
    let index = reqwest::get(format!("{}/", app.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        index.contains("<a href=\"/privacy\">Privacy</a>"),
        "{index}"
    );
}

#[sqlx::test]
async fn the_privacy_contact_is_configurable(pool: PgPool) {
    let app = common::start_configured(pool, |c| {
        c.contact = "ladder@example.org".to_owned();
        c.replay_hours = 48;
    })
    .await;
    let html = reqwest::get(format!("{}/privacy", app.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("<a href=\"mailto:ladder@example.org\">ladder@example.org</a>"),
        "{html}"
    );
    assert!(html.contains("48 hours"));
}
