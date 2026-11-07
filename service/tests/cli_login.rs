//! The `kfm` CLI's login (API v1.3, `docs/ladder-api.md` §5.1) end to end
//! with the fake providers: the page sends a signed-out browser through the
//! sign-in and back, the confirmation form (CSRF) creates a `submit` token
//! named after the host and redirects to the CLI's listener with the state and
//! a one-time code, and the exchange turns the code into the token once.

mod common;

use common::{error_code, location, start, Browser, FakeUser, Provider, TestApp};
use reqwest::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;

const PORT: u16 = 49_321;
const STATE: &str = "St4te-of_the.art~0123456789";

fn page(host: &str) -> String {
    format!("/auth/cli?port={PORT}&state={STATE}&host={host}")
}

/// The value of `name="<field>" value="..."` in a page.
fn field(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html
        .find(&marker)
        .unwrap_or_else(|| panic!("no {name} in {html}"))
        + marker.len();
    let end = html[start..].find('"').unwrap() + start;
    html[start..end].to_owned()
}

/// A query parameter of a URL.
fn param(url: &str, name: &str) -> Option<String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

async fn exchange(app: &TestApp, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/auth/cli/exchange", app.base))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Confirms on the page as a signed-in browser; the CLI listener URL it
/// redirected to.
async fn allow(browser: &mut Browser, host: &str) -> String {
    let html = browser.get(&page(host)).await.text().await.unwrap();
    let csrf = field(&html, "csrf");
    let response = browser
        .post_form(
            "/auth/cli",
            &[
                ("csrf", &csrf),
                ("port", &PORT.to_string()),
                ("state", STATE),
                ("host", host),
                ("decision", "allow"),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    location(&response)
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
    n
}

#[sqlx::test]
async fn a_signed_out_browser_signs_in_confirms_and_the_cli_gets_a_submit_token(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = Browser::new(&app);

    // Signed out: the page offers both sign-ins, each coming back to it.
    let response = browser.get(&page("laptop")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        csp.contains("form-action 'self' http://127.0.0.1:*"),
        "{csp}"
    );
    let html = response.text().await.unwrap();
    let next: String = url::form_urlencoded::byte_serialize(page("laptop").as_bytes()).collect();
    let github = format!("/auth/github?next={next}");
    assert!(
        html.contains(&github.replace('&', "&amp;")) || html.contains(&github),
        "{html}"
    );
    assert!(html.contains("/auth/google?next="), "{html}");
    assert!(!html.contains("name=\"csrf\""));

    // The sign-in returns to the page, not to the portal.
    app.fake
        .set_next(Provider::GitHub, FakeUser::new("7001", "clier"));
    let start = browser.get(&github).await;
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    let at_provider = browser.get(&location(&start)).await;
    let done = browser.get(&location(&at_provider)).await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&done), page("laptop"));

    // Signed in: the confirmation names the token.
    let html = browser.get(&page("laptop")).await.text().await.unwrap();
    assert!(
        html.contains("Authorize the kfm CLI on this computer?"),
        "{html}"
    );
    assert!(html.contains("kfm on laptop"), "{html}");
    assert_eq!(field(&html, "state"), STATE);
    assert_eq!(field(&html, "port"), PORT.to_string());

    let callback = allow(&mut browser, "laptop").await;
    assert!(
        callback.starts_with(&format!("http://127.0.0.1:{PORT}/callback?")),
        "{callback}"
    );
    assert_eq!(param(&callback, "state").as_deref(), Some(STATE));
    let code = param(&callback, "code").expect("a code");
    assert_eq!(code.len(), 22, "128 bits in base64url");
    assert!(!callback.contains("kfm_"), "no secret in the URL");

    // Only the code's hash is stored, and the token exists already.
    use sha2::Digest;
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        code.as_bytes(),
    )
    .unwrap();
    let (stored,): (Vec<u8>,) = sqlx::query_as("SELECT hash FROM cli_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, sha2::Sha256::digest(&raw).to_vec());

    let response = exchange(&app, json!({ "code": code })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["name"], "kfm on laptop");
    assert_eq!(body["scope"], "submit");
    let secret = body["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("kfm_") && secret.len() == 47, "{secret}");
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "{body}");

    // The token is the browser user's, listed on the portal, and works.
    let tokens: serde_json::Value = browser.get("/tokens").await.json().await.unwrap();
    assert_eq!(tokens["tokens"][0]["id"], body["token_id"]);
    assert_eq!(tokens["tokens"][0]["name"], "kfm on laptop");
    let me: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/me", app.base))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["auth"], "token");
    assert_eq!(me["scope"], "submit");
    assert_eq!(me["display_name"], "clier");

    // A code is single use.
    assert_eq!(
        error_code(exchange(&app, json!({ "code": code })).await).await,
        "invalid_code"
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM cli_codes").await, 0);
}

#[sqlx::test]
async fn an_expired_code_is_refused_and_its_token_revoked(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = common::github_browser(&app, 7002, "late").await;
    let code = param(&allow(&mut browser, "desk").await, "code").unwrap();
    sqlx::query("UPDATE cli_codes SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        error_code(exchange(&app, json!({ "code": code })).await).await,
        "invalid_code"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM tokens WHERE revoked_at IS NULL"
        )
        .await,
        0
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM cli_codes").await, 0);

    // A later confirmation sweeps uncollected expired codes the same way.
    let _ = allow(&mut browser, "desk").await;
    sqlx::query("UPDATE cli_codes SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let fresh = param(&allow(&mut browser, "desk").await, "code").unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM cli_codes").await, 1);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM tokens WHERE revoked_at IS NULL"
        )
        .await,
        1
    );
    let response = exchange(&app, json!({ "code": fresh })).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[sqlx::test]
async fn malformed_and_unknown_codes_are_refused(pool: PgPool) {
    let app = start(pool).await;
    for code in ["", "nope", "AAAAAAAAAAAAAAAAAAAAAA", "kfm_x"] {
        assert_eq!(
            error_code(exchange(&app, json!({ "code": code })).await).await,
            "invalid_code",
            "{code:?}"
        );
    }
    assert_eq!(
        error_code(exchange(&app, json!({ "nocode": 1 })).await).await,
        "bad_request"
    );
}

#[sqlx::test]
async fn the_confirmation_needs_the_session_and_its_csrf_token(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = common::github_browser(&app, 7003, "guarded").await;
    let form = |csrf: &'static str| {
        vec![
            ("csrf", csrf),
            ("port", "49321"),
            ("state", STATE),
            ("host", "x"),
            ("decision", "allow"),
        ]
    };
    for csrf in ["", "wrong"] {
        let response = browser.post_form("/auth/cli", &form(csrf)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/?error=bad_csrf");
    }
    let mut stranger = Browser::new(&app);
    let response = stranger.post_form("/auth/cli", &form("")).await;
    assert_eq!(location(&response), "/?error=unauthenticated");
    assert_eq!(count(&pool, "SELECT count(*) FROM tokens").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM cli_codes").await, 0);

    // A cancel goes back to the CLI with the state and no code.
    let csrf = browser.csrf().await;
    let response = browser
        .post_form(
            "/auth/cli",
            &[
                ("csrf", &csrf),
                ("port", "49321"),
                ("state", STATE),
                ("decision", "deny"),
            ],
        )
        .await;
    let back = location(&response);
    assert_eq!(param(&back, "state").as_deref(), Some(STATE));
    assert_eq!(param(&back, "error").as_deref(), Some("access_denied"));
    assert_eq!(param(&back, "code"), None);
    assert_eq!(count(&pool, "SELECT count(*) FROM tokens").await, 0);

    // A form with a bad port never reaches a listener.
    let response = browser
        .post_form(
            "/auth/cli",
            &[
                ("csrf", &csrf),
                ("port", "80"),
                ("state", STATE),
                ("decision", "allow"),
            ],
        )
        .await;
    assert_eq!(location(&response), "/?error=bad_request");
}

#[sqlx::test]
async fn a_bad_link_is_an_error_page_and_a_bad_next_is_ignored(pool: PgPool) {
    let app = start(pool).await;
    let mut browser = Browser::new(&app);
    for bad in [
        "/auth/cli",
        "/auth/cli?port=80&state=abcdefgh",
        "/auth/cli?port=5000&state=abc",
        "/auth/cli?port=5000&state=ab%2Fcdefgh",
        "/auth/cli?port=x&state=abcdefgh",
    ] {
        let response = browser.get(bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        let html = response.text().await.unwrap();
        assert!(html.contains("not valid"), "{bad}: {html}");
    }

    // A next that is not the CLI page lands on the portal as before.
    app.fake
        .set_next(Provider::GitHub, FakeUser::new("7004", "nexter"));
    let start = browser.get("/auth/github?next=https://evil.example/").await;
    let at_provider = browser.get(&location(&start)).await;
    let done = browser.get(&location(&at_provider)).await;
    assert_eq!(location(&done), "/");
    // The session works and the JSON routes are unchanged by the flow.
    let me = browser
        .json(Method::GET, "/me", None, None)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(me["display_name"], "nexter");
}

#[sqlx::test]
async fn the_token_limit_is_reported_to_the_cli(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut browser = common::github_browser(&app, 7005, "hoarder").await;
    for _ in 0..25 {
        common::new_token(&mut browser, "read").await;
    }
    let back = allow(&mut browser, "full").await;
    assert_eq!(param(&back, "state").as_deref(), Some(STATE));
    assert_eq!(param(&back, "error").as_deref(), Some("limit_reached"));
    assert_eq!(param(&back, "code"), None);
}
