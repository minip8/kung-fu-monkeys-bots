//! The portal: the signed-out and signed-in pages, the shell every page is
//! framed by, and the form handlers with their CSRF check and where they
//! land.

mod common;

use common::{github_browser, location, start, Browser, FakeUser, Provider};
use reqwest::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

/// What a browser's navigation sends (`GET /tokens` answers it the page).
const HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

async fn get_page(browser: &mut Browser, path: &str) -> reqwest::Response {
    let request = browser
        .http
        .get(format!("{}{path}", browser.base))
        .header("accept", HTML);
    browser.send(request).await
}

async fn page(browser: &mut Browser, path: &str) -> String {
    let response = get_page(browser, path).await;
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
    // The icon is the one thing a page loads, from the service itself.
    assert!(headers["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("img-src 'self'"));
    assert!(html.contains("<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">"));
    assert!(html.contains("<link rel=\"icon\" href=\"/favicon.ico\" sizes=\"32x32\">"));
    assert!(html.contains("href=\"/auth/github\""));
    assert!(html.contains("href=\"/auth/google\""));
    assert!(!html.contains("<script"));
    assert!(!html.contains("name=\"csrf\""));
    // The landing page: what the contest is, the way in, the guide, and an
    // empty board before any season.
    assert!(html.contains("Bank the bananas."), "{html}");
    assert!(html.contains("kfm submit --wait --activate"));
    assert!(html.contains("href=\"/docs/contest\""));
    assert!(html.contains("<svg class=\"map\""), "the grove drawing");
    assert!(html.contains("No team has played in this season yet."));
    assert!(html.contains("No game has been played yet."));
    // The live panels, empty and saying so, and the four arenas.
    assert!(html.contains("<h2>Now playing</h2>"));
    assert!(
        html.contains("No game is being played right now: no season is running."),
        "{html}"
    );
    assert!(html.contains("Games are scheduled while a season runs."));
    assert_eq!(
        html.matches("<svg class=\"map\"").count(),
        5,
        "the hero's and the four cards"
    );
    assert!(html.contains("href=\"/docs/contest#arenas\""));
}

#[sqlx::test]
async fn the_contest_guide_is_public_and_names_its_engine_release(pool: PgPool) {
    let guide = kfm_ladder::portal::CONTEST_GUIDE_ENGINE;
    // The test configuration plays another release than the guide's: it says so.
    let app = start(pool.clone()).await;
    let response = reqwest::get(format!("{}/docs/contest", app.base))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'none'"));
    let html = response.text().await.unwrap();
    assert!(!html.contains("<script"));
    assert!(html.contains("The contest: bananas are the economy"));
    assert!(html.contains(&format!("engine release <strong>{guide}</strong>")));
    assert!(
        html.contains("The ladder now plays engine release v0.5.1."),
        "{html}"
    );
    assert_eq!(
        html.matches("<svg class=\"map\"").count(),
        4,
        "the pool's four arenas"
    );
    for arena in ["grove", "orchards", "gorge", "burrows"] {
        assert!(html.contains(&format!("<b>{arena}</b>")), "{arena}");
    }
    // The ladder's own numbers come from its configuration.
    assert!(html.contains("Every 5 minutes"), "{html}");
    assert!(html.contains("for 24 hours,"), "{html}");
    // Every page's navigation and footer link to it.
    let index = reqwest::get(format!("{}/", app.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(index.contains("<a class=\"nav-i n-guide\" href=\"/docs/contest\">"));

    // On the guide's own release there is no notice.
    let pinned = common::start_configured(pool, |c| {
        c.engine_tag = guide.to_owned();
        c.round_seconds = 1800;
        c.replay_hours = 0;
    })
    .await;
    let html = reqwest::get(format!("{}/docs/contest", pinned.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("The ladder now plays"), "{html}");
    assert!(html.contains("Every 30 minutes"), "{html}");
    assert!(!html.contains("hours,"), "replays kept for ever: {html}");
}

#[sqlx::test]
async fn signed_in_shows_the_account_and_a_link_button(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4001, "portal-user").await;
    let html = page(&mut browser, "/account").await;
    assert!(html.contains("<h1>portal-user</h1>"), "{html}");
    assert!(html.contains("portal-user@example.org"));
    assert!(html.contains("href=\"/auth/google\">Link Google"));
    assert!(!html.contains("Link GitHub"));
    assert_eq!(csrf_in(&html), browser.csrf().await);

    let landed = browser
        .oauth(&app, Provider::Google, FakeUser::new("g-p", "Portal G"))
        .await;
    assert_eq!(landed, "/account?linked=google");
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
    let csrf = csrf_in(&page(&mut browser, "/tokens").await);
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

    assert!(
        html.contains("aria-current=\"page\""),
        "the token page itself"
    );
    let reloaded = page(&mut browser, "/tokens").await;
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
    assert_eq!(location(&response), "/tokens");
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert!(listed["tokens"].as_array().unwrap().is_empty());
}

#[sqlx::test]
async fn a_form_without_the_csrf_token_changes_nothing(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = github_browser(&app, 4003, "victim").await;
    for (path, form, back) in [
        (
            "/portal/tokens",
            vec![("name", "evil"), ("scope", "submit")],
            "/tokens",
        ),
        ("/portal/teams", vec![("name", "Evil Team")], "/team"),
    ] {
        let mut with_wrong = form.clone();
        with_wrong.push(("csrf", "forged"));
        for fields in [form, with_wrong] {
            let response = browser.post_form(path, &fields).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(location(&response), format!("{back}?error=bad_csrf"));
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
    let csrf = csrf_in(&page(&mut owner, "/team").await);

    let response = owner
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "Form Team")])
        .await;
    assert_eq!(location(&response), "/team");
    let response = owner
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "x")])
        .await;
    assert_eq!(location(&response), "/team?error=invalid_name");
    let html = page(&mut owner, "/team?error=invalid_name").await;
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
    assert_eq!(location(&response), "/team");

    let mate_html = page(&mut mate, "/team").await;
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
    assert_eq!(location(&response), "/team");
    assert!(!page(&mut mate, "/team").await.contains("Form Team"));

    let owner_id = owner.user_id().await;
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/members/{owner_id}/remove"),
            &[("csrf", &csrf)],
        )
        .await;
    assert_eq!(location(&response), "/team?error=last_owner");
}

#[sqlx::test]
async fn the_rename_form_is_an_owners(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 4040, "namer").await;
    let mut mate = github_browser(&app, 4041, "named").await;
    let mut rival = github_browser(&app, 4042, "rival").await;
    let team = common::new_team(&mut owner, "First Draft").await;
    common::new_team(&mut rival, "Taken Name").await;
    let csrf = csrf_in(&page(&mut owner, "/team").await);
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/members"),
            &[("csrf", &csrf), ("user", "named")],
        )
        .await;
    assert_eq!(location(&response), "/team");

    let action = format!("action=\"/portal/teams/{team}/rename\"");
    assert!(page(&mut owner, "/team").await.contains(&action));
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/rename"),
            &[("csrf", &csrf), ("name", "Final Cut")],
        )
        .await;
    assert_eq!(location(&response), "/team");
    let html = page(&mut owner, "/team").await;
    assert!(html.contains("Final Cut"));
    assert!(!html.contains("First Draft"));

    // A refused name comes back as its code's message and changes nothing.
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/rename"),
            &[("csrf", &csrf), ("name", "taken name")],
        )
        .await;
    assert_eq!(location(&response), "/team?error=name_taken");
    let html = page(&mut owner, "/team?error=name_taken").await;
    assert!(html.contains("That team name is taken."));
    assert!(html.contains("Final Cut"));
    let response = owner
        .post_form(
            &format!("/portal/teams/{team}/rename"),
            &[("csrf", "not-it"), ("name", "Forged")],
        )
        .await;
    assert_eq!(location(&response), "/team?error=bad_csrf");

    // A member sees the new name and no form, and the route refuses them.
    let mate_html = page(&mut mate, "/team").await;
    assert!(mate_html.contains("Final Cut"));
    assert!(!mate_html.contains(&action));
    let mate_csrf = csrf_in(&mate_html);
    let response = mate
        .post_form(
            &format!("/portal/teams/{team}/rename"),
            &[("csrf", &mate_csrf), ("name", "Mutiny")],
        )
        .await;
    assert_eq!(location(&response), "/team?error=forbidden");
    assert!(page(&mut owner, "/team").await.contains("Final Cut"));
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
    let html = page(&mut browser, "/submissions").await;
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
    assert_eq!(location(&response), "/submissions?error=bad_csrf");

    // A refused archive comes back with what was wrong.
    let bad = common::zip_of(&[("../x.c", b"x")]);
    let request = browser.http.post(&path).multipart(form(&csrf, bad));
    let response = browser.send(request).await;
    let landed = location(&response);
    assert!(
        landed.starts_with("/submissions?error=bad_source&detail="),
        "{landed}"
    );
    let html = page(&mut browser, &landed).await;
    assert!(
        html.contains("The source archive is refused: &#34;../x.c&#34; contains a .. component."),
        "{html}"
    );
    // A detail is shown only for the codes that carry one.
    let html = page(&mut browser, "/submissions?error=forbidden&detail=injected").await;
    assert!(!html.contains("injected"));

    let request = browser
        .http
        .post(&path)
        .multipart(form(&csrf, common::tiny_bot_zip()));
    let response = browser.send(request).await;
    assert_eq!(location(&response), "/submissions");
    let (id, state): (uuid::Uuid, String) = sqlx::query_as("SELECT id, state FROM submissions")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(state, "queued");
    let html = page(&mut browser, "/submissions").await;
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
    let html = page(&mut browser, "/submissions").await;
    assert!(html.contains(&format!("href=\"/submissions/{id}/log\"")));
    assert!(html.contains("<code title=\"SHA-256 of the module\">abababababab</code>"));
    assert!(html.contains(&format!("name=\"submission\" value=\"{id}\"")));
    let response = browser
        .post_form(
            &format!("/portal/teams/{team}/active"),
            &[("csrf", &csrf), ("submission", &id.to_string())],
        )
        .await;
    assert_eq!(location(&response), "/submissions");
    let html = page(&mut browser, "/submissions").await;
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

#[sqlx::test]
async fn the_create_team_form_is_for_an_account_without_a_team(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut solo = github_browser(&app, 4030, "single").await;
    let html = page(&mut solo, "/team").await;
    assert!(html.contains("action=\"/portal/teams\""));
    let csrf = csrf_in(&html);
    let response = solo
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "Only Team")])
        .await;
    assert_eq!(location(&response), "/team");

    // In a team: no form, and the route refuses with the rule's message.
    let html = page(&mut solo, "/team").await;
    assert!(html.contains("Only Team"));
    assert!(!html.contains("action=\"/portal/teams\""));
    let response = solo
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "Another")])
        .await;
    assert_eq!(location(&response), "/team?error=already_in_team");
    let html = page(&mut solo, "/team?error=already_in_team").await;
    assert!(html.contains("at most one team"));

    // An operator keeps the form.
    kfm_ladder::users::set_operator(&pool, "single", true)
        .await
        .unwrap();
    let html = page(&mut solo, "/team").await;
    assert!(html.contains("action=\"/portal/teams\""));
    let response = solo
        .post_form("/portal/teams", &[("csrf", &csrf), ("name", "Another")])
        .await;
    assert_eq!(location(&response), "/team");
}

#[sqlx::test]
async fn the_favicon_is_served_as_svg_and_ico(pool: PgPool) {
    let app = start(pool).await;
    let svg = reqwest::get(format!("{}/favicon.svg", app.base))
        .await
        .unwrap();
    assert_eq!(svg.status(), StatusCode::OK);
    assert_eq!(svg.headers()["content-type"], "image/svg+xml");
    assert_eq!(svg.headers()["cache-control"], "public, max-age=86400");
    assert_eq!(svg.headers()["x-content-type-options"], "nosniff");
    let text = svg.text().await.unwrap();
    assert!(text.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
    assert!(!text.contains("<script"));
    // The mark is the header's: the same banana on every page.
    let path = text
        .split("<path d=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the icon has a path");
    let index = reqwest::get(format!("{}/", app.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(index.contains(path), "the header draws the icon's banana");

    let ico = reqwest::get(format!("{}/favicon.ico", app.base))
        .await
        .unwrap();
    assert_eq!(ico.status(), StatusCode::OK);
    assert_eq!(ico.headers()["content-type"], "image/x-icon");
    let bytes = ico.bytes().await.unwrap();
    // An ICO directory: reserved 0, type 1 (icon), then the image count.
    assert_eq!(&bytes[..4], &[0, 0, 1, 0]);
    assert_eq!(bytes[4], 3, "16, 32 and 48 pixels");
}

/// The account's pages and the sidebar entry each marks as current.
const ACCOUNT_PAGES: [(&str, &str); 5] = [
    ("/", "n-home"),
    ("/team", "n-team"),
    ("/submissions", "n-submissions"),
    ("/tokens", "n-tokens"),
    ("/account", "me"),
];

#[sqlx::test]
async fn the_account_pages_send_a_signed_out_browser_home(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    for path in ["/team", "/submissions", "/tokens", "/account"] {
        let response = get_page(&mut browser, path).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(location(&response), "/", "{path}");
    }
    // A program asking for `GET /tokens` still gets the JSON error.
    let response = browser.get("/tokens").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(common::error_code(response).await, "unauthenticated");
}

#[sqlx::test]
async fn each_account_page_renders_in_the_shell_and_marks_itself(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4050, "shell-user").await;
    let csrf = browser.csrf().await;
    for (path, entry) in ACCOUNT_PAGES {
        let html = page(&mut browser, path).await;
        // The sidebar: the name linking to the account, and a sign-out form.
        assert!(
            html.contains("<span>shell-user</span></a>"),
            "{path}: {html}"
        );
        assert!(html.contains("<form method=\"post\" action=\"/auth/logout\">"));
        assert_eq!(csrf_in(&html), csrf, "{path}");
        assert!(!html.contains(">Sign in</a>"), "{path}");
        // Exactly the page's own entry is current.
        assert_eq!(html.matches("aria-current=\"page\"").count(), 1, "{path}");
        let marked = html
            .split("aria-current=\"page\"")
            .next()
            .unwrap()
            .rsplit('<')
            .next()
            .unwrap();
        assert!(marked.contains(entry), "{path}: {marked}");
        for link in [
            "/docs/contest",
            "/team",
            "/ladder",
            "/matches",
            "/submissions",
            "/tokens",
        ] {
            assert!(html.contains(&format!("href=\"{link}\"")), "{path} {link}");
        }
        assert!(html.contains("href=\"https://kfm.minip8.com\""));
        assert!(!html.contains("<script"));
        // Every page shows `?error=` as its message.
        let shown = page(&mut browser, &format!("{path}?error=oauth_state")).await;
        assert!(shown.contains("The sign-in expired"), "{path}");
    }
}

#[sqlx::test]
async fn a_bearer_asking_for_html_still_gets_the_token_list(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4051, "bearer-html").await;
    let csrf = csrf_in(&page(&mut browser, "/tokens").await);
    let response = browser
        .post_form(
            "/portal/tokens",
            &[("csrf", &csrf), ("name", "ci"), ("scope", "read")],
        )
        .await;
    let html = response.text().await.unwrap();
    let start = html.find("kfm_").unwrap();
    let secret = &html[start..start + 47];
    // The secret is never in a link or the sidebar: once, in the notice.
    assert_eq!(html.matches(secret).count(), 1);
    // A refused name comes back to the token page with its message.
    let response = browser
        .post_form(
            "/portal/tokens",
            &[("csrf", &csrf), ("name", "x"), ("scope", "admin")],
        )
        .await;
    assert_eq!(location(&response), "/tokens?error=invalid_scope");
    // A program with a session token gets JSON whatever its Accept says...
    let listed = reqwest::Client::new()
        .get(format!("{}/tokens", app.base))
        .header("accept", HTML)
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    // ...here `session_required`: a token cannot list tokens, as before.
    assert_eq!(listed.status(), StatusCode::FORBIDDEN);
    assert_eq!(common::error_code(listed).await, "session_required");
    // And without Accept, the session gets the JSON list.
    let listed: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert_eq!(listed["tokens"][0]["name"], "ci");
}

#[sqlx::test]
async fn the_home_is_a_dashboard_of_the_account(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = github_browser(&app, 4052, "dash").await;
    // No team: a prompt to create one, and no account figures.
    let html = page(&mut browser, "/").await;
    assert!(html.contains("Welcome back, dash"), "{html}");
    assert!(html.contains("No team yet"));
    assert!(html.contains(">Create a team</a>"), "{html}");
    assert!(!html.contains("class=\"spark\""));
    assert!(!html.contains("Bank the bananas."), "not the landing page");
    // The live panels, the about strip and the arenas are there all the same.
    assert!(html.contains("<h2>Now playing</h2>") && html.contains("<h2>Latest results</h2>"));
    assert!(html.contains("href=\"/docs/contest#arenas\""));
    assert_eq!(
        html.matches("<svg class=\"map\"").count(),
        4,
        "the four arenas"
    );
    // A team with no game: not rated, no active bot, the way to submit one.
    common::new_team(&mut browser, "Dash Team").await;
    let html = page(&mut browser, "/").await;
    assert!(html.contains("<h2>Dash Team</h2>"), "{html}");
    assert!(html.contains("Not rated in the current season yet"));
    assert!(
        html.contains("<li class=\"chip bad\">No active bot</li>"),
        "{html}"
    );
    assert!(html.contains("href=\"/submissions\">Submit a bot</a>"));
    assert!(html.contains("href=\"/team\">Team</a>"));
    assert!(!html.contains("class=\"spark\""));
    assert!(html.contains("No ranked game yet"), "the team's games");
}

/// A recorded game between `a` and `b` in `season`, `minutes` ago, side A
/// winning with rating changes `delta_a` and `-delta_a`.
async fn played(
    pool: &PgPool,
    season: Uuid,
    (a, sa): (Uuid, Uuid),
    (b, sb): (Uuid, Uuid),
    delta_a: f64,
    minutes: i32,
) {
    sqlx::query(
        "INSERT INTO matches (id, season_id, seed, engine_tag, content_hash, team_a, submission_a, \
           team_b, submission_b, winner_team, result, round, score_a, score_b, delta_a, delta_b, created_at) \
         VALUES ($1, $2, 1, 'v0.11.0', 'a99dfcf8', $3, $4, $5, $6, $3, '{\"arena\": \"grove\"}', 1, \
           40, 12, $7, -$7, now() - make_interval(mins => $8))",
    )
    .bind(Uuid::now_v7())
    .bind(season)
    .bind(a)
    .bind(sa)
    .bind(b)
    .bind(sb)
    .bind(delta_a)
    .bind(minutes)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn the_home_draws_the_teams_rating_this_season(pool: PgPool) {
    let app = start(pool.clone()).await;
    let season = common::db_season(&pool, "Spark", 30).await;
    let mut browser = github_browser(&app, 4054, "sparky").await;
    let mine: Uuid = common::new_team(&mut browser, "Spark Team")
        .await
        .parse()
        .unwrap();
    let mine_bot = common::db_ready_bot(&pool, None, mine, b"mine").await;
    let other = common::db_team(&pool, "Other Team").await;
    let other_bot = common::db_ready_bot(&pool, None, other, b"other").await;
    // Three games: +20, -8, +12 from 1500, so 1524 now; the other team the mirror.
    for (i, delta) in [20.0, -8.0, 12.0].into_iter().enumerate() {
        played(
            &pool,
            season.id,
            (mine, mine_bot),
            (other, other_bot),
            delta,
            30 - i as i32 * 10,
        )
        .await;
    }
    for (team, mu, wdl) in [(mine, 24.0, (2, 0, 1)), (other, -24.0, (1, 0, 2))] {
        sqlx::query(
            "INSERT INTO ratings (season_id, team_id, mu, phi, sigma, games, wins, draws, losses) \
             VALUES ($1, $2, $3, 0.5, 0.06, 3, $4, $5, $6)",
        )
        .bind(season.id)
        .bind(team)
        .bind(mu / kfm_ladder::glicko::SCALE)
        .bind(wdl.0)
        .bind(wdl.1)
        .bind(wdl.2)
        .execute(&pool)
        .await
        .unwrap();
    }
    let html = page(&mut browser, "/").await;
    assert!(
        html.contains("<b>1524<small>± 87</small></b><span>Rating</span>"),
        "{html}"
    );
    assert!(
        html.contains("<b class=\"rank\">#1</b><span>Rank · Spark</span>"),
        "{html}"
    );
    // The sparkline walks back from 1524 through the three changes: four
    // points across the box, the highest (now) at the top.
    assert!(
        html.contains("aria-label=\"Rating over 3 games this season, from 1500 to 1524\""),
        "{html}"
    );
    let points = html
        .split("<polyline class=\"ln\" points=\"")
        .nth(1)
        .unwrap();
    let points = &points[..points.find('"').unwrap()];
    assert_eq!(points.split(' ').count(), 4, "{points}");
    assert!(
        points.starts_with("0.0,52.0 ") && points.ends_with(" 200.0,4.0"),
        "{points}"
    );
    assert!(
        html.contains("<b class=\"good\">+24.0</b> over the last 3"),
        "{html}"
    );
    assert!(
        html.contains("<b class=\"good\">2</b> won · <b>0</b> drawn · <b class=\"bad\">1</b> lost")
    );
    assert!(
        html.contains("<li class=\"chip good\">Active bot · "),
        "{html}"
    );
    // The team's games, and the ladder's latest results with the team's side marked.
    assert!(html.contains("Spark Team's latest games"));
    assert_eq!(
        html.matches("<td class=\"team\">Other Team</td>").count(),
        3,
        "{html}"
    );
    assert!(html.contains("<h2>Latest results</h2>"));
    assert!(html.matches("href=\"/matches/").count() >= 6, "{html}");
}

#[sqlx::test]
async fn now_playing_lists_the_match_jobs_by_team(pool: PgPool) {
    let app = start(pool.clone()).await;
    let season = common::db_season(&pool, "Live", 30).await;
    // Signed out, with nothing queued: an honest empty state, no round known yet.
    let mut anonymous = Browser::new(&app);
    let html = page(&mut anonymous, "/").await;
    assert!(
        html.contains("No game is being played right now."),
        "{html}"
    );
    assert!(!html.contains("Next round"), "{html}");
    assert!(html.contains("as of "));

    let mut browser = github_browser(&app, 4055, "live").await;
    let mine: Uuid = common::new_team(&mut browser, "Live Team")
        .await
        .parse()
        .unwrap();
    let teams = [
        mine,
        common::db_team(&pool, "Second Team").await,
        common::db_team(&pool, "Third Team").await,
    ];
    for (i, (a, b, state)) in [(0, 1, "running"), (1, 2, "queued")]
        .into_iter()
        .enumerate()
    {
        let payload = serde_json::json!({"season": season.id, "round": 4, "mode": "contest",
            "team_a": teams[a], "submission_a": Uuid::now_v7(), "team_b": teams[b],
            "submission_b": Uuid::now_v7(), "seed": i});
        sqlx::query("INSERT INTO jobs (id, kind, payload, state) VALUES ($1, 'match', $2, $3)")
            .bind(Uuid::now_v7())
            .bind(payload)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
    }
    // A finished job and a job of another kind are not listed.
    sqlx::query("INSERT INTO jobs (id, kind, payload, state) VALUES ($1, 'match', '{}', 'done'), ($2, 'build', '{}', 'queued')")
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rounds (season_id, number, teams, pairs, created_at) VALUES ($1, 4, 3, 2, now() - interval '2 minutes')")
        .bind(season.id)
        .execute(&pool)
        .await
        .unwrap();
    let html = page(&mut browser, "/").await;
    assert_eq!(html.matches("<li class=\"np-line").count(), 2, "{html}");
    assert!(html.contains("<li class=\"np-line mine\">"), "{html}");
    assert!(
        html.contains("<span class=\"a me\">Live Team</span>"),
        "{html}"
    );
    assert!(html.contains("<span class=\"b\">Second Team</span>"));
    assert!(html.contains("<span class=\"a\">Second Team</span>"));
    assert!(html.contains("<span class=\"chip live\">Live</span>"));
    assert!(html.contains("<span class=\"chip\">Queued</span>"));
    assert!(html.contains(" · round 4"));
    // The test app's rounds are 5 minutes apart; the last was 2 ago.
    assert!(html.contains("<b>Next round in about 3 min</b>"), "{html}");
    // The running game is listed first, and a visitor sees the same.
    assert!(html.find("chip live").unwrap() < html.find("chip\">Queued").unwrap());
    let html = page(&mut anonymous, "/").await;
    assert_eq!(html.matches("<li class=\"np-line\">").count(), 2, "{html}");
}

#[sqlx::test]
async fn the_public_pages_know_who_is_signed_in(pool: PgPool) {
    let app = start(pool).await;
    let mut anonymous = Browser::new(&app);
    let mut signed_in = github_browser(&app, 4053, "visitor").await;
    for (path, current) in [
        ("/ladder", Some("n-ladder\" href=\"/ladder\"")),
        ("/matches", Some("n-games\" href=\"/matches\"")),
        ("/docs/contest", Some("n-guide\" href=\"/docs/contest\"")),
        ("/privacy", None),
        ("/auth/cli?port=5000&state=abcdefgh", None),
    ] {
        let html = page(&mut anonymous, path).await;
        assert!(html.contains(">Sign in</a>"), "{path}");
        assert!(!html.contains("action=\"/auth/logout\""), "{path}");
        let html = page(&mut signed_in, path).await;
        assert!(html.contains("<span>visitor</span></a>"), "{path}");
        assert!(html.contains("action=\"/auth/logout\""), "{path}");
        match current {
            Some(entry) => assert!(
                html.contains(&format!("{entry} aria-current=\"page\"")),
                "{path}"
            ),
            None => assert!(!html.contains("aria-current=\"page\""), "{path}"),
        }
    }
}
