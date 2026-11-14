//! The ladder's public reads: `GET /ladder` (the board, paged, by season),
//! `GET /matches` (filters and paging), `GET /matches/{m}` (the raw result,
//! the replay and viewer links), the team object's `rating`, and their
//! portal pages for a browser.

mod common;

use common::{db_ready_bot, db_season, db_team, error_code, github_browser, new_team, start};
use kfm_ladder::glicko::Rating;
use reqwest::StatusCode;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

async fn rate(pool: &PgPool, season: Uuid, team: Uuid, r: f64, rd: f64, wdl: (i32, i32, i32)) {
    let rating = Rating::from_glicko(r, rd, 0.06);
    sqlx::query(
        "INSERT INTO ratings (season_id, team_id, mu, phi, sigma, games, wins, draws, losses) \
         VALUES ($1, $2, $3, $4, 0.06, $5, $6, $7, $8)",
    )
    .bind(season)
    .bind(team)
    .bind(rating.mu)
    .bind(rating.phi)
    .bind(wdl.0 + wdl.1 + wdl.2)
    .bind(wdl.0)
    .bind(wdl.1)
    .bind(wdl.2)
    .execute(pool)
    .await
    .unwrap();
}

struct Side {
    team: Uuid,
    submission: Uuid,
}

/// A recorded game, `minutes` ago; side A won when `a_won`, else a draw.
async fn db_match(
    pool: &PgPool,
    season: Uuid,
    a: &Side,
    b: &Side,
    a_won: bool,
    minutes: i32,
) -> Uuid {
    let id = Uuid::now_v7();
    // A won game's runner named its arena; a drawn one's (as an engine
    // release before v0.10.0) did not.
    let mut result = json!({"match": 0, "seed": 42, "ticks": 2000, "checksum": "0badf00d",
        "result": "ended", "winner": if a_won { json!(0) } else { Value::Null },
        "winner_team": if a_won { json!(0) } else { Value::Null }, "teams": [], "slots": [],
        "content_hash": "a99dfcf8", "build_hash": "00000000"});
    if a_won {
        result["arena"] = json!("gorge");
    }
    sqlx::query(
        "INSERT INTO matches (id, season_id, seed, engine_tag, content_hash, build_hash, team_a, submission_a, \
           team_b, submission_b, winner_team, replay_key, result, round, game, side_a, ending, ticks, \
           score_a, kills_a, deaths_a, faults_a, delta_a, score_b, kills_b, deaths_b, faults_b, delta_b, created_at) \
         VALUES ($1, $2, 42, 'v0.5.1', 'a99dfcf8', '00000000', $3, $4, $5, $6, $7, $8, $9, 1, 0, 0, \
           'ended', 2000, 3, 9, 2, '[]', 12.34, 1, 2, 9, $10, -12.34, now() - make_interval(mins => $11))",
    )
    .bind(id)
    .bind(season)
    .bind(a.team)
    .bind(a.submission)
    .bind(b.team)
    .bind(b.submission)
    .bind(a_won.then_some(a.team))
    .bind(format!("replays/{id}.kfmr"))
    .bind(result)
    .bind(json!([{"slot": 1, "traps": 4, "over": 0, "bad": 0, "dead": "trap"}]))
    .bind(minutes)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn get(base: &str, path: &str) -> reqwest::Response {
    reqwest::get(format!("{base}{path}")).await.unwrap()
}

async fn get_json(base: &str, path: &str) -> Value {
    let response = get(base, path).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    response.json().await.unwrap()
}

async fn get_html(base: &str, path: &str) -> String {
    let response = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'none'"));
    response.text().await.unwrap()
}

#[sqlx::test]
async fn the_board_ranks_the_current_season_and_pages(pool: PgPool) {
    let app = start(pool.clone()).await;
    // Before any season: an empty board.
    let empty = get_json(&app.base, "/ladder").await;
    assert_eq!(
        empty,
        json!({"season": null, "page": 1, "ratings": [], "next": null})
    );

    let now = time::OffsetDateTime::now_utc();
    let old = kfm_ladder::seasons::create(
        &pool,
        "Old",
        now - time::Duration::days(60),
        now - time::Duration::days(30),
        "contest",
    )
    .await
    .unwrap();
    let season = db_season(&pool, "Now", 30).await;
    // 55 rated teams (ratings 1000..1540 by 10), one unrated, one rated in
    // the old season only, and one with a row but no game yet.
    let mut ids = Vec::new();
    for i in 0..55 {
        let team = db_team(&pool, &format!("Team {i:02}")).await;
        rate(
            &pool,
            season.id,
            team,
            1000.0 + 10.0 * i as f64,
            80.4,
            (i, 1, 2),
        )
        .await;
        ids.push(team);
    }
    let bot_team = db_team(&pool, "Active one").await;
    let sub = db_ready_bot(&pool, None, bot_team, b"module").await;
    rate(&pool, season.id, bot_team, 2000.0, 49.6, (3, 0, 0)).await;
    db_team(&pool, "Unrated").await;
    let old_only = db_team(&pool, "Old timers").await;
    rate(&pool, old.id, old_only, 1900.0, 60.0, (5, 0, 0)).await;
    let gameless = db_team(&pool, "No games").await;
    sqlx::query(
        "INSERT INTO ratings (season_id, team_id, mu, phi, sigma) VALUES ($1, $2, 9.0, 2.0, 0.06)",
    )
    .bind(season.id)
    .bind(gameless)
    .execute(&pool)
    .await
    .unwrap();

    let board = get_json(&app.base, "/ladder").await;
    assert_eq!(board["season"]["name"], "Now");
    assert_eq!(board["season"]["current"], true);
    assert_eq!(board["page"], 1);
    assert_eq!(board["next"], 2);
    let rows = board["ratings"].as_array().unwrap();
    assert_eq!(rows.len(), 50);
    let top = &rows[0];
    assert_eq!(top["rank"], 1);
    assert_eq!(top["team_id"], bot_team.to_string());
    assert_eq!(top["name"], "Active one");
    assert_eq!(top["rating"], 2000);
    assert_eq!(top["rd"], 50);
    assert_eq!(
        (
            top["games"].clone(),
            top["wins"].clone(),
            top["draws"].clone(),
            top["losses"].clone()
        ),
        (json!(3), json!(3), json!(0), json!(0))
    );
    assert_eq!(top["active"], sub.to_string());
    assert_eq!(
        top["module_hash"],
        hex::encode(kfm_ladder::crypto::sha256(b"module"))
    );
    assert_eq!(rows[1]["name"], "Team 54");
    assert_eq!(rows[1]["rating"], 1540);
    assert_eq!(rows[1]["rd"], 80);
    assert_eq!(rows[1]["active"], Value::Null);
    assert_eq!(rows[1]["module_hash"], Value::Null);
    for key in [
        "rank",
        "team_id",
        "name",
        "rating",
        "rd",
        "games",
        "wins",
        "draws",
        "losses",
        "active",
        "module_hash",
    ] {
        assert!(top.get(key).is_some(), "{key}");
    }
    let second = get_json(&app.base, "/ladder?page=2").await;
    let rest = second["ratings"].as_array().unwrap();
    assert_eq!(rest.len(), 6);
    assert_eq!(rest[0]["rank"], 51);
    assert_eq!(rest[5]["name"], "Team 00");
    assert_eq!(second["next"], Value::Null);
    let names: Vec<&str> = rows
        .iter()
        .chain(rest)
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"Unrated")
            && !names.contains(&"Old timers")
            && !names.contains(&"No games")
    );

    // Another season by name, an unknown one, a bad page.
    let past = get_json(&app.base, "/ladder?season=Old").await;
    assert_eq!(past["season"]["current"], false);
    assert_eq!(past["ratings"][0]["name"], "Old timers");
    assert_eq!(past["ratings"].as_array().unwrap().len(), 1);
    let unknown = get(&app.base, "/ladder?season=Nope").await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(unknown).await, "not_found");
    let bad = get(&app.base, "/ladder?page=0").await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // The team object carries its current standing.
    let team = get_json(&app.base, &format!("/teams/{}", ids[54])).await;
    assert_eq!(
        team["rating"],
        json!({"season": "Now", "rank": 2, "rating": 1540, "rd": 80, "games": 57, "wins": 54, "draws": 1, "losses": 2})
    );
    let unrated = get_json(&app.base, &format!("/teams/{old_only}")).await;
    assert_eq!(unrated["rating"], Value::Null);

    // The portal's board.
    let html = get_html(&app.base, "/ladder").await;
    assert!(html.contains("Season <strong>Now</strong>"), "{html}");
    assert!(html.contains("Active one") && html.contains("Team 54"));
    assert!(!html.contains("Team 00"), "page 1 holds 50");
    assert!(
        html.contains("href=\"/ladder?page=2&#38;season=Now\""),
        "{html}"
    );
    assert!(html.contains("href=\"/ladder?season=Old\""), "{html}");
    let page2 = get_html(&app.base, "/ladder?page=2").await;
    assert!(page2.contains("Team 00") && page2.contains("previous"));
    // A team's name links to its games.
    assert!(
        html.contains(&format!(
            "href=\"/matches?team={}\" title=\"This team's games\">Team 54</a>",
            ids[54]
        )),
        "{html}"
    );
    // The landing page shows the top of the current season's board.
    let landing = get_html(&app.base, "/").await;
    assert!(landing.contains("Season Now"), "{landing}");
    assert!(landing.contains("Active one") && landing.contains("Team 54"));
    assert!(landing.contains("Team 51") && !landing.contains("Team 50"));
}

#[sqlx::test]
async fn matches_filter_by_team_and_season_and_page(pool: PgPool) {
    let app = start(pool.clone()).await;
    let now = time::OffsetDateTime::now_utc();
    let old = kfm_ladder::seasons::create(
        &pool,
        "Old",
        now - time::Duration::days(60),
        now - time::Duration::days(30),
        "contest",
    )
    .await
    .unwrap();
    let season = db_season(&pool, "Now", 30).await;
    let mut sides = Vec::new();
    for name in ["Alpha", "Beta", "Gamma"] {
        let team = db_team(&pool, name).await;
        let submission = db_ready_bot(&pool, None, team, name.as_bytes()).await;
        sides.push(Side { team, submission });
    }
    // Alpha v Beta 30 times this season (newest last made), Beta v Gamma
    // twice, and Alpha v Gamma once last season.
    let mut ab = Vec::new();
    for i in 0..30 {
        ab.push(db_match(&pool, season.id, &sides[0], &sides[1], i % 2 == 0, 100 - i).await);
    }
    db_match(&pool, season.id, &sides[1], &sides[2], true, 5).await;
    db_match(&pool, season.id, &sides[1], &sides[2], false, 4).await;
    let older = db_match(&pool, old.id, &sides[0], &sides[2], true, 90_000).await;

    let all = get_json(&app.base, "/matches").await;
    assert_eq!(all["matches"].as_array().unwrap().len(), 25);
    assert_eq!(all["next"], 2);
    let p2 = get_json(&app.base, "/matches?page=2").await;
    assert_eq!(p2["matches"].as_array().unwrap().len(), 8);
    assert_eq!(p2["next"], Value::Null);
    assert_eq!(p2["matches"][7]["id"], older.to_string(), "oldest last");

    let alpha = get_json(&app.base, &format!("/matches?team={}", sides[0].team)).await;
    assert_eq!(alpha["matches"].as_array().unwrap().len(), 25);
    let alpha2 = get_json(
        &app.base,
        &format!("/matches?team={}&page=2", sides[0].team),
    )
    .await;
    assert_eq!(alpha2["matches"].as_array().unwrap().len(), 6);
    let gamma = get_json(&app.base, &format!("/matches?team={}", sides[2].team)).await;
    assert_eq!(gamma["matches"].as_array().unwrap().len(), 3);
    let gamma_now = get_json(
        &app.base,
        &format!("/matches?team={}&season=Now", sides[2].team),
    )
    .await;
    assert_eq!(gamma_now["matches"].as_array().unwrap().len(), 2);
    let gamma_old = get_json(
        &app.base,
        &format!("/matches?team={}&season=Old", sides[2].team),
    )
    .await;
    assert_eq!(gamma_old["matches"][0]["id"], older.to_string());
    assert_eq!(
        get(&app.base, "/matches?team=nope").await.status(),
        StatusCode::BAD_REQUEST
    );

    // The same list as a page for a browser, with the same filters.
    let html = get_html(&app.base, "/matches").await;
    assert!(html.contains("<h1>Games</h1>"), "{html}");
    assert!(html.contains("Every ranked game"), "{html}");
    assert_eq!(html.matches(">game</a>").count(), 25);
    assert!(
        html.contains("<td class=\"team\">Beta</td><td class=\"num\"><strong>3 – 1</strong></td><td>Gamma</td>"),
        "the winner in bold: {html}"
    );
    assert!(html.contains("<code>gorge</code>"), "{html}");
    assert!(html.contains("href=\"/matches?page=2\">next</a>"), "{html}");
    let html = get_html(
        &app.base,
        &format!("/matches?team={}&season=Now&page=1", sides[2].team),
    )
    .await;
    assert!(html.contains("<h1>Games of Gamma</h1>"), "{html}");
    assert!(html.contains("of season <strong>Now</strong>"), "{html}");
    assert_eq!(html.matches(">game</a>").count(), 2);
    assert!(!html.contains(">next</a>") && !html.contains(">previous</a>"));
    let alpha2 = get_html(
        &app.base,
        &format!("/matches?team={}&page=2", sides[0].team),
    )
    .await;
    assert!(
        alpha2.contains(&format!(
            "href=\"/matches?page=1&#38;team={}\">previous</a>",
            sides[0].team
        )),
        "{alpha2}"
    );
    let bad = reqwest::Client::new()
        .get(format!("{}/matches?season=Nope", app.base))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::NOT_FOUND);
    // The board's page lists the season's latest games, and the landing page too.
    let board = get_html(&app.base, "/ladder").await;
    assert!(board.contains("Latest games"), "{board}");
    assert!(
        board.contains("href=\"/matches?season=Now\">All games</a>"),
        "{board}"
    );
    assert_eq!(board.matches("class=\"game\"").count(), 8);
    let landing = get_html(&app.base, "/").await;
    assert_eq!(landing.matches("class=\"game\"").count(), 5);
    assert_eq!(
        get(&app.base, "/matches?season=Nope").await.status(),
        StatusCode::NOT_FOUND
    );

    // The summary's shape: the newest Alpha v Beta game, side A won.
    let newest = &alpha["matches"][0];
    assert_eq!(newest["id"], ab[29].to_string());
    let summary_keys = [
        "id",
        "season",
        "season_id",
        "round",
        "game",
        "created_at",
        "engine_tag",
        "seed",
        "ticks",
        "end",
        "winner",
        "sides",
        "arena",
    ];
    assert_eq!(
        newest.as_object().unwrap().len(),
        summary_keys.len(),
        "{newest}"
    );
    for key in summary_keys {
        assert!(newest.get(key).is_some(), "{key}");
    }
    assert_eq!(newest["season"], "Now");
    assert_eq!(newest["round"], 1);
    assert_eq!(newest["engine_tag"], "v0.5.1");
    assert_eq!(newest["end"], "ended");
    assert_eq!(newest["winner"], Value::Null, "i = 29 is a draw");
    let a = &newest["sides"][0];
    assert_eq!(a["team_id"], sides[0].team.to_string());
    assert_eq!(a["name"], "Alpha");
    assert_eq!(a["submission_id"], sides[0].submission.to_string());
    assert_eq!(
        (
            a["team"].clone(),
            a["score"].clone(),
            a["kills"].clone(),
            a["deaths"].clone()
        ),
        (json!(0), json!(3), json!(9), json!(2))
    );
    assert_eq!(a["faults"], json!([]));
    assert_eq!(a["rating_change"], 12.3);
    let b = &newest["sides"][1];
    assert_eq!(b["team"], 1);
    assert_eq!(b["faults"][0]["dead"], "trap");
    assert_eq!(b["rating_change"], -12.3);
    assert_eq!(alpha["matches"][1]["winner"], sides[0].team.to_string());
    // The arena is the stored result's (v1.6); null when it names none.
    assert_eq!(newest["arena"], Value::Null);
    assert_eq!(alpha["matches"][1]["arena"], "gorge");

    // One match: the raw result, the replay and the viewer.
    let detail = get_json(&app.base, &format!("/matches/{}", ab[29])).await;
    assert_eq!(detail["id"], ab[29].to_string());
    assert_eq!(detail["sides"], newest["sides"]);
    assert_eq!(detail["content_hash"], "a99dfcf8");
    assert_eq!(detail["build_hash"], "00000000");
    assert_eq!(detail["result"]["checksum"], "0badf00d");
    let replay = detail["replay_url"].as_str().unwrap();
    assert!(
        replay.contains(&format!("/replays/{}.kfmr?", ab[29])),
        "{replay}"
    );
    assert!(replay.contains("X-Amz-Expires=3600"), "{replay}");
    let viewer = detail["viewer_url"].as_str().unwrap();
    let encoded: String = url::form_urlencoded::byte_serialize(replay.as_bytes()).collect();
    assert_eq!(
        viewer,
        format!("https://kfm.minip8.com/v0.5.1/?replay={encoded}")
    );
    assert_eq!(
        get(&app.base, &format!("/matches/{}", Uuid::now_v7()))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app.base, "/matches/nope").await.status(),
        StatusCode::NOT_FOUND
    );

    // Its portal page.
    let html = get_html(&app.base, &format!("/matches/{}", ab[28])).await;
    assert!(html.contains("Alpha v Beta"), "{html}");
    assert!(html.contains("Alpha won"), "{html}");
    assert!(html.contains("on <code>gorge</code>"), "{html}");
    assert!(html.contains("arena gorge, seed 42"), "{html}");
    assert!(!html.contains("of its pair"), "{html}");
    assert!(html.contains("slot 1: traps 4, dead (trap)"), "{html}");
    assert!(html.contains("+12.3") && html.contains("-12.3"));
    assert!(
        html.contains("https://kfm.minip8.com/v0.5.1/?replay="),
        "{html}"
    );
    assert!(
        html.contains("&#34;checksum&#34;: &#34;0badf00d&#34;"),
        "{html}"
    );
}

#[sqlx::test]
async fn the_portal_shows_a_teams_standing_and_recent_games(pool: PgPool) {
    let app = start(pool.clone()).await;
    let season = db_season(&pool, "Now", 30).await;
    let mut browser = github_browser(&app, 9101, "ranker").await;
    let mine: Uuid = new_team(&mut browser, "Mine").await.parse().unwrap();
    let my_sub = db_ready_bot(&pool, None, mine, b"mine").await;
    let theirs = db_team(&pool, "<b>Theirs</b>").await;
    let their_sub = db_ready_bot(&pool, None, theirs, b"theirs").await;
    rate(&pool, season.id, mine, 1612.0, 91.0, (1, 0, 1)).await;
    rate(&pool, season.id, theirs, 1388.0, 91.0, (1, 0, 1)).await;
    let me = Side {
        team: mine,
        submission: my_sub,
    };
    let them = Side {
        team: theirs,
        submission: their_sub,
    };
    let won = db_match(&pool, season.id, &me, &them, true, 3).await;
    let lost = db_match(&pool, season.id, &them, &me, true, 2).await;
    let response = browser.get("/").await;
    let html = response.text().await.unwrap();
    assert!(
        html.contains("Season Now: rank 1, rating 1612 ± 91, 2 game(s): 1 won, 0 drawn, 1 lost."),
        "{html}"
    );
    assert!(html.contains(&format!("href=\"/matches/{won}\"")));
    assert!(html.contains(&format!("href=\"/matches/{lost}\"")));
    assert!(
        html.contains("&#60;b&#62;Theirs&#60;/b&#62;"),
        "names are escaped: {html}"
    );
    assert!(!html.contains("<b>Theirs</b>"));
    // Side B of `lost`, whose slot 1 faulted: the fault shows on my line.
    assert!(
        html.contains("lost <span class=\"bad\">fault</span>"),
        "{html}"
    );
    assert!(html.contains("href=\"/ladder\""));
}
