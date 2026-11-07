//! Teams: create, list, show, add members, leave and remove, the last-owner rule.

mod common;

use common::{error_code, github_browser, start, Browser};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;

async fn create_team(browser: &mut Browser, name: &str) -> reqwest::Response {
    let csrf = browser.csrf().await;
    browser
        .json(
            Method::POST,
            "/teams",
            Some(json!({ "name": name })),
            Some(&csrf),
        )
        .await
}

async fn add(browser: &mut Browser, team: &str, user: &str) -> reqwest::Response {
    let csrf = browser.csrf().await;
    browser
        .json(
            Method::POST,
            &format!("/teams/{team}/members"),
            Some(json!({ "user": user })),
            Some(&csrf),
        )
        .await
}

async fn remove(browser: &mut Browser, team: &str, user: &str) -> reqwest::Response {
    let csrf = browser.csrf().await;
    browser
        .json(
            Method::DELETE,
            &format!("/teams/{team}/members/{user}"),
            None,
            Some(&csrf),
        )
        .await
}

async fn new_team(browser: &mut Browser, name: &str) -> String {
    let response = create_team(browser, name).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let team: Value = response.json().await.unwrap();
    team["id"].as_str().unwrap().to_owned()
}

#[sqlx::test]
async fn the_creator_owns_a_new_team(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 3001, "captain").await;
    let owner_id = owner.user_id().await;
    let response = create_team(&mut owner, "  Banana Squad ").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let team: Value = response.json().await.unwrap();
    assert_eq!(team["name"], "Banana Squad");
    assert_eq!(team["members"].as_array().unwrap().len(), 1);
    assert_eq!(team["members"][0]["user_id"], owner_id.as_str());
    assert_eq!(team["members"][0]["display_name"], "captain");
    assert_eq!(team["members"][0]["role"], "owner");

    let mine: Value = owner.get("/teams").await.json().await.unwrap();
    assert_eq!(mine["teams"][0]["id"], team["id"]);
    assert_eq!(mine["teams"][0]["role"], "owner");

    // A team is public.
    let id = team["id"].as_str().unwrap();
    let shown: Value = reqwest::get(format!("{}/teams/{id}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown, team);
}

#[sqlx::test]
async fn an_account_starts_without_a_team(pool: PgPool) {
    let app = start(pool).await;
    let mut solo = github_browser(&app, 3002, "solo").await;
    let mine: Value = solo.get("/teams").await.json().await.unwrap();
    assert_eq!(mine, json!({ "teams": [] }));
}

#[sqlx::test]
async fn team_names_are_checked_and_unique_ignoring_case(pool: PgPool) {
    let app = start(pool).await;
    let mut a = github_browser(&app, 3003, "namer").await;
    for bad in ["ab", "x".repeat(33).as_str(), "<b>hi</b>", " -dash"] {
        let response = create_team(&mut a, bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(error_code(response).await, "invalid_name");
    }
    new_team(&mut a, "Monkeys").await;
    let mut b = github_browser(&app, 3004, "copier").await;
    let response = create_team(&mut b, "mONKEYS").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "name_taken");
}

#[sqlx::test]
async fn team_writes_need_the_session_and_its_csrf_token(pool: PgPool) {
    let app = start(pool).await;
    let mut a = github_browser(&app, 3005, "forgetful").await;
    let response = a
        .json(
            Method::POST,
            "/teams",
            Some(json!({ "name": "No Csrf" })),
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "bad_csrf");
    let response = a
        .json(
            Method::POST,
            "/teams",
            Some(json!({ "name": "Bad Csrf" })),
            Some("not-it"),
        )
        .await;
    assert_eq!(error_code(response).await, "bad_csrf");
    let anonymous = reqwest::Client::new()
        .post(format!("{}/teams", app.base))
        .json(&json!({ "name": "Nobody" }))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(anonymous).await, "unauthenticated");
}

#[sqlx::test]
async fn an_owner_adds_members_by_id_or_name(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 3006, "boss").await;
    let mut by_id = github_browser(&app, 3007, "first-mate").await;
    let mut by_name = github_browser(&app, 3008, "Deckhand").await;
    let team = new_team(&mut owner, "Crew").await;

    let id = by_id.user_id().await;
    let response = add(&mut owner, &team, &id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = add(&mut owner, &team, "deckhand").await;
    assert_eq!(response.status(), StatusCode::OK);
    let shown: Value = response.json().await.unwrap();
    let roles: Vec<(&str, &str)> = shown["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["display_name"].as_str().unwrap(),
                m["role"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("boss", "owner"),
            ("first-mate", "member"),
            ("Deckhand", "member")
        ]
    );

    let mine: Value = by_name.get("/teams").await.json().await.unwrap();
    assert_eq!(mine["teams"][0]["role"], "member");

    let again = add(&mut owner, &team, &id).await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(again).await, "already_member");
    let nobody = add(&mut owner, &team, "nobody-here").await;
    assert_eq!(nobody.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(nobody).await, "unknown_user");
}

#[sqlx::test]
async fn a_shared_name_is_ambiguous(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 3009, "host").await;
    let mut one = Browser::new(&app);
    one.sign_in(
        &app,
        common::Provider::Google,
        common::FakeUser::new("g-twin-1", "Twin"),
    )
    .await;
    let mut two = Browser::new(&app);
    two.sign_in(
        &app,
        common::Provider::Google,
        common::FakeUser::new("g-twin-2", "twin"),
    )
    .await;
    let team = new_team(&mut owner, "Twins").await;
    let response = add(&mut owner, &team, "Twin").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "ambiguous_user");
    let id = two.user_id().await;
    assert_eq!(add(&mut owner, &team, &id).await.status(), StatusCode::OK);
}

#[sqlx::test]
async fn only_an_owner_adds_members(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 3010, "owner").await;
    let mut member = github_browser(&app, 3011, "member").await;
    let mut outsider = github_browser(&app, 3012, "outsider").await;
    let team = new_team(&mut owner, "Closed").await;
    let member_id = member.user_id().await;
    add(&mut owner, &team, &member_id).await;

    let outsider_id = outsider.user_id().await;
    let response = add(&mut member, &team, &outsider_id).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(response).await, "forbidden");
    let response = add(&mut outsider, &team, &outsider_id).await;
    assert_eq!(error_code(response).await, "forbidden");
    let missing = add(
        &mut owner,
        "0190f0f0-0000-7000-8000-000000000000",
        &outsider_id,
    )
    .await;
    assert_eq!(error_code(missing).await, "not_found");
}

#[sqlx::test]
async fn members_leave_and_owners_remove_but_the_last_owner_stays(pool: PgPool) {
    let app = start(pool).await;
    let mut owner = github_browser(&app, 3013, "anchor").await;
    let mut stays = github_browser(&app, 3014, "stays").await;
    let mut leaves = github_browser(&app, 3015, "leaves").await;
    let team = new_team(&mut owner, "Ship").await;
    let owner_id = owner.user_id().await;
    let stays_id = stays.user_id().await;
    let leaves_id = leaves.user_id().await;
    add(&mut owner, &team, &stays_id).await;
    add(&mut owner, &team, &leaves_id).await;

    // A member cannot remove another member.
    let response = remove(&mut stays, &team, &leaves_id).await;
    assert_eq!(error_code(response).await, "forbidden");
    // A member leaves.
    let response = remove(&mut leaves, &team, &leaves_id).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mine: Value = leaves.get("/teams").await.json().await.unwrap();
    assert_eq!(mine, json!({ "teams": [] }));
    let response = remove(&mut leaves, &team, &leaves_id).await;
    assert_eq!(error_code(response).await, "not_found");
    // The owner removes a member.
    let response = remove(&mut owner, &team, &stays_id).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // The last owner cannot leave, and nobody removes them.
    let response = remove(&mut owner, &team, &owner_id).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(response).await, "last_owner");
    let response = remove(&mut stays, &team, &owner_id).await;
    assert_eq!(error_code(response).await, "forbidden");

    let shown: Value = reqwest::get(format!("{}/teams/{team}", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["members"].as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn an_owner_may_leave_while_another_owner_remains(pool: PgPool) {
    let app = start(pool.clone()).await;
    let mut first = github_browser(&app, 3016, "first").await;
    let mut second = github_browser(&app, 3017, "second").await;
    let team = new_team(&mut first, "Duo").await;
    let first_id = first.user_id().await;
    let second_id = second.user_id().await;
    add(&mut first, &team, &second_id).await;
    // There is no promotion route in v1.0; promote in the database.
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE user_id = $1::uuid")
        .bind(&second_id)
        .execute(&pool)
        .await
        .unwrap();
    let response = remove(&mut first, &team, &first_id).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = remove(&mut second, &team, &second_id).await;
    assert_eq!(error_code(response).await, "last_owner");
}

#[sqlx::test]
async fn an_unknown_team_is_not_found(pool: PgPool) {
    let app = start(pool).await;
    let response = reqwest::get(format!(
        "{}/teams/0190f0f0-0000-7000-8000-000000000000",
        app.base
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "not_found");
}
