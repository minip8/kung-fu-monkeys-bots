//! The per-client limit of the presigning routes (API v1.4):
//! `GET /matches/{m}` (JSON and HTML) and `GET /submissions/{s}` share a
//! bucket of `KFM_LADDER_PRESIGN_PER_MINUTE` per client IP; the 61st request
//! in a minute is `429 rate_limited` with `Retry-After`; `X-Forwarded-For`
//! names the client only behind a trusted proxy.

mod common;

use common::{start_configured, TestApp};
use reqwest::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

async fn get(app: &TestApp, path: &str, forwarded: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("{}{path}", app.base));
    if let Some(ip) = forwarded {
        request = request.header("x-forwarded-for", ip);
    }
    request.send().await.unwrap()
}

fn a_match() -> String {
    format!("/matches/{}", Uuid::now_v7())
}

async fn assert_limited(response: reqwest::Response) {
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "rate_limited");
    assert!(body["message"].is_string());
    assert_eq!(body.as_object().unwrap().len(), 2);
}

#[sqlx::test]
async fn the_61st_request_in_a_minute_is_refused(pool: PgPool) {
    let app = start_configured(pool, |c| c.presign_per_minute = 60).await;
    // Unknown matches answer 404 quickly; the limit counts them all, and an
    // untrusted X-Forwarded-For changes nothing (every request is the peer's).
    for i in 0..60 {
        let forged = format!("198.51.100.{i}");
        let response = get(&app, &a_match(), Some(&forged)).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "request {}",
            i + 1
        );
    }
    assert_limited(get(&app, &a_match(), Some("198.51.100.200")).await).await;
    // The HTML page and the submission route share the bucket; the limit
    // runs before authentication.
    let html = reqwest::Client::new()
        .get(format!("{}{}", app.base, a_match()))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_limited(html).await;
    assert_limited(get(&app, &format!("/submissions/{}", Uuid::now_v7()), None).await).await;
    // Routes that presign nothing are not limited.
    assert_eq!(get(&app, "/healthz", None).await.status(), StatusCode::OK);
    assert_eq!(get(&app, "/matches", None).await.status(), StatusCode::OK);
}

#[sqlx::test]
async fn behind_a_trusted_proxy_each_forwarded_client_has_its_bucket(pool: PgPool) {
    let app = start_configured(pool, |c| {
        c.presign_per_minute = 60;
        c.trust_proxy = true;
    })
    .await;
    for _ in 0..60 {
        let response = get(&app, &a_match(), Some("198.51.100.1, 10.0.0.1")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_limited(get(&app, &a_match(), Some("198.51.100.1")).await).await;
    // Another client is unaffected, and so is the peer without the header.
    assert_eq!(
        get(&app, &a_match(), Some("198.51.100.2")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, &format!("/submissions/{}", Uuid::now_v7()), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn zero_turns_the_limit_off(pool: PgPool) {
    let app = start_configured(pool, |c| c.presign_per_minute = 0).await;
    for _ in 0..100 {
        assert_eq!(
            get(&app, &a_match(), None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
