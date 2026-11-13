//! Every command: the configuration resolved, the ladder called, the answer
//! printed (a summary, or the raw JSON with `--json`). People's output goes
//! to stdout; progress and diagnostics to stderr.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::{enc, Api};
use crate::arena::{self, Plan};
use crate::config::{self, Config, Effective};
use crate::error::{other, Context, Error, Result};
use crate::table::{s, short, table, when};
use crate::{login, pack, recipe, wasm};
use crate::{BotsCmd, BuildArgs, Cli, Cmd, MatchesCmd, ReplayCmd, SubmitArgs, TestArgs};

/// How long `--wait` waits for a submission to be final.
const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

struct Ctx {
    path: PathBuf,
    file: Config,
    eff: Effective,
    json: bool,
}

impl Ctx {
    fn api(&self) -> Result<Api> {
        Api::new(&self.eff.server, self.eff.token.clone())
    }

    /// The API with a token, or the "not logged in" error.
    fn authed(&self) -> Result<Api> {
        if self.eff.token.is_none() {
            return Err(Error::Api {
                code: "unauthenticated".to_owned(),
                message: format!(
                    "not logged in to {}: run kfm login (or set KFM_TOKEN)",
                    self.eff.server
                ),
            });
        }
        self.api()
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

pub fn run(cli: Cli) -> Result<()> {
    let path = config::config_path()?;
    let file = Config::load(&path)?;
    let env_token = std::env::var("KFM_TOKEN").ok();
    let eff = config::resolve(cli.server.as_deref(), env_token, &file);
    let ctx = Ctx {
        path,
        file,
        eff,
        json: cli.json,
    };
    match cli.command {
        Cmd::Login { no_browser } => cmd_login(&ctx, no_browser),
        Cmd::Logout => cmd_logout(&ctx),
        Cmd::Whoami => cmd_whoami(&ctx),
        Cmd::Build(args) => cmd_build(&ctx, &args),
        Cmd::Test(args) => cmd_test(&ctx, &args),
        Cmd::Submit(args) => cmd_submit(&ctx, &args),
        Cmd::Bots(BotsCmd::List { team }) => cmd_bots_list(&ctx, team.as_deref()),
        Cmd::Bots(BotsCmd::Activate { submission, team }) => {
            cmd_bots_activate(&ctx, &submission, team.as_deref())
        }
        Cmd::Bots(BotsCmd::Log { submission }) => cmd_bots_log(&ctx, &submission),
        Cmd::Matches(MatchesCmd::List { team, season, page }) => {
            cmd_matches_list(&ctx, team.as_deref(), season.as_deref(), page)
        }
        Cmd::Matches(MatchesCmd::Show { id }) => cmd_match_show(&ctx, &id),
        Cmd::Ladder { season, page } => cmd_ladder(&ctx, season.as_deref(), page),
        Cmd::Seasons => cmd_seasons(&ctx),
        Cmd::Replay(ReplayCmd::Get { id, out }) => cmd_replay_get(&ctx, &id, out),
        Cmd::Replay(ReplayCmd::Open { id, no_browser }) => cmd_replay_open(&ctx, &id, no_browser),
    }
}

// --- login, logout, whoami -----------------------------------------------------------------

fn cmd_login(ctx: &Ctx, no_browser: bool) -> Result<()> {
    let server = ctx.eff.server.clone();
    let token = login::login(&server, no_browser)?;
    let saved = Config {
        server: Some(server.clone()),
        token: Some(token.token.clone()),
        token_id: Some(token.id.clone()),
        token_name: Some(token.name.clone()),
    };
    saved.save(&ctx.path)?;
    let who = Api::new(&server, Some(token.token))
        .and_then(|api| api.get("/me"))
        .map(|me| s(&me, "display_name"))
        .unwrap_or_else(|_| "?".to_owned());
    println!(
        "Logged in to {server} as {who}: {} token '{}' stored in {}",
        token.scope,
        token.name,
        ctx.path.display()
    );
    if ctx.eff.token_from_env {
        eprintln!("kfm: note: KFM_TOKEN is set and overrides the stored token");
    }
    Ok(())
}

fn cmd_logout(ctx: &Ctx) -> Result<()> {
    if ctx.eff.token_from_env {
        return Err(Error::Usage(
            "KFM_TOKEN is set: unset it (kfm logout forgets only the token kfm login stored)"
                .to_owned(),
        ));
    }
    let Some(token) = ctx.eff.token.clone() else {
        println!("Not logged in to {}", ctx.eff.server);
        return Ok(());
    };
    if let Some(id) = &ctx.eff.token_id {
        let api = Api::new(&ctx.eff.server, Some(token))?;
        match api.delete(&format!("/tokens/{id}")) {
            Ok(()) => println!(
                "Revoked token '{}'",
                ctx.file.token_name.as_deref().unwrap_or(id)
            ),
            // Revoked already (on the portal): nothing left to do.
            Err(Error::Api { code, .. }) if code == "invalid_token" || code == "not_found" => {}
            Err(err) => eprintln!(
                "kfm: could not revoke the token ({err}); revoke it on the portal, {}/",
                ctx.eff.server
            ),
        }
    } else {
        eprintln!(
            "kfm: the token's id is unknown, so it is not revoked; revoke it on the portal, {}/",
            ctx.eff.server
        );
    }
    let mut file = ctx.file.clone();
    file.token = None;
    file.token_id = None;
    file.token_name = None;
    file.save(&ctx.path)?;
    println!("Logged out of {}", ctx.eff.server);
    Ok(())
}

fn cmd_whoami(ctx: &Ctx) -> Result<()> {
    let api = ctx.authed()?;
    let me = api.get("/me")?;
    let teams = api.get("/teams")?;
    if ctx.json {
        print_json(&json!({ "me": me, "teams": teams["teams"] }));
        return Ok(());
    }
    println!(
        "{} (user {}) on {}",
        s(&me, "display_name"),
        s(&me, "id"),
        ctx.eff.server
    );
    let source = if ctx.eff.token_from_env {
        "KFM_TOKEN".to_owned()
    } else {
        format!(
            "token '{}' in {}",
            ctx.file.token_name.as_deref().unwrap_or("?"),
            ctx.path.display()
        )
    };
    println!("Authenticated by {source}, scope {}", s(&me, "scope"));
    let list = teams["teams"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No team yet: create one on the portal, {}/", ctx.eff.server);
    } else {
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|t| vec![s(t, "name"), s(t, "role"), s(t, "id")])
            .collect();
        print!("{}", table(&["TEAM", "ROLE", "ID"], &rows));
    }
    Ok(())
}

// --- teams ----------------------------------------------------------------------------------

fn is_uuid(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The team named or identified by `given` among the caller's teams, or the
/// only one; `(id, name)`.
fn resolve_team(api: &Api, server: &str, given: Option<&str>) -> Result<(String, String)> {
    let teams = api.get("/teams")?;
    let list = teams["teams"].as_array().cloned().unwrap_or_default();
    let names = || {
        list.iter()
            .map(|t| s(t, "name"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let pick = |t: &Value| (s(t, "id"), s(t, "name"));
    match given {
        Some(g) => list
            .iter()
            .find(|t| s(t, "id") == g || s(t, "name").eq_ignore_ascii_case(g))
            .map(pick)
            .ok_or_else(|| {
                Error::Usage(if list.is_empty() {
                    format!("you are in no team named {g:?} (in no team at all)")
                } else {
                    format!("you are in no team named {g:?}; yours: {}", names())
                })
            }),
        None => match list.as_slice() {
            [] => Err(Error::Usage(format!(
                "you are in no team: create one on the portal, {server}/"
            ))),
            [one] => Ok(pick(one)),
            many => Err(Error::Usage(format!(
                "you are in {} teams ({}): pass --team NAME or ID (ambiguous)",
                many.len(),
                names()
            ))),
        },
    }
}

// --- build and test -------------------------------------------------------------------------

fn cmd_build(ctx: &Ctx, args: &BuildArgs) -> Result<()> {
    if !args.dir.is_dir() {
        return Err(Error::Usage(format!(
            "no such bot directory: {}",
            args.dir.display()
        )));
    }
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.dir.join("out").join("bot.wasm"));
    if args.local {
        recipe::build_local(&args.dir, &out)?;
    } else {
        let image = args.image.clone().unwrap_or_else(recipe::default_image);
        let log = out.with_file_name("build.log");
        recipe::build_in_image(&args.dir, &out, &image, &log)?;
    }
    let (bytes, sha) = recipe::check_module(&out)?;
    if ctx.json {
        print_json(&json!({ "module": out, "bytes": bytes, "sha256": sha }));
    } else {
        println!("{}: {bytes} bytes, sha256 {sha}", out.display());
    }
    Ok(())
}

/// The engine release `kfm test` plays: the flag, else the ladder's (when
/// logged in), else this binary's pin.
fn engine_tag(ctx: &Ctx, given: Option<&str>) -> String {
    if let Some(tag) = given {
        return tag.to_owned();
    }
    if ctx.eff.token.is_some() {
        match ctx.api().and_then(|api| api.get("/seasons")) {
            Ok(body) => {
                if let Some(tag) = body["engine_tag"].as_str().filter(|t| !t.is_empty()) {
                    return tag.to_owned();
                }
            }
            Err(err) => eprintln!(
                "kfm: could not ask the ladder for its engine release ({err}); using {}",
                arena::PINNED_ENGINE_TAG
            ),
        }
    }
    arena::PINNED_ENGINE_TAG.to_owned()
}

fn spec_of(path: &Path) -> Result<String> {
    let text = path.to_string_lossy().into_owned();
    if text.contains(',') {
        return Err(Error::Usage(format!(
            "{text}: a module path may not contain a comma (kfm_arena's --team separates with commas)"
        )));
    }
    Ok(text)
}

fn cmd_test(ctx: &Ctx, args: &TestArgs) -> Result<()> {
    if args.matches == 0 {
        return Err(Error::Usage("--matches must be at least 1".to_owned()));
    }
    let module = &args.module;
    let bytes = std::fs::read(module).map_err(|_| {
        Error::Usage(format!(
            "no module at {}: build one with kfm build, or name it",
            module.display()
        ))
    })?;
    wasm::check(&bytes).map_err(|why| other(format!("{}: {why}", module.display())))?;
    let against = if args.against.starts_with("ref") {
        args.against.clone()
    } else {
        let path = Path::new(&args.against);
        if !path.is_file() {
            return Err(Error::Usage(format!(
                "--against {}: neither ref:easy|medium|hard nor a module file",
                args.against
            )));
        }
        spec_of(path)?
    };
    let (arena_path, tag) = match &args.arena {
        Some(path) => (path.clone(), "local".to_owned()),
        None => {
            let tag = engine_tag(ctx, args.engine_tag.as_deref());
            (arena::runner(&ctx.api()?, &tag)?, tag)
        }
    };
    let plan = Plan {
        module: spec_of(module)?,
        against,
        matches: args.matches,
        mode: args
            .mode
            .clone()
            .or_else(|| (!args.ffa).then(|| "contest".to_owned())),
        ffa: args.ffa,
        seed: args.seed,
        ticks: args.ticks,
    };
    let objects = arena::run(&arena_path, &arena::arena_args(&plan))?;
    let summary = arena::summarize(&objects, plan.ffa)?;
    let text = arena::render(&summary, &plan, &tag);
    if ctx.json {
        for o in &objects {
            println!("{o}");
        }
        eprint!("{text}");
    } else {
        print!("{text}");
    }
    if summary.faulted() {
        return Err(other(
            "the module faulted (a trap, a tick over budget, a bad return or a dead slot): the \
             ladder's validation rejects a module that faults in its smoke match",
        ));
    }
    Ok(())
}

// --- submissions ----------------------------------------------------------------------------

fn poll_interval() -> Duration {
    std::env::var("KFM_POLL_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(3), Duration::from_millis)
}

const FINAL_STATES: [&str; 3] = ["ready", "failed", "rejected"];

fn cmd_submit(ctx: &Ctx, args: &SubmitArgs) -> Result<()> {
    if !args.dir.is_dir() {
        return Err(Error::Usage(format!(
            "no such bot directory: {}",
            args.dir.display()
        )));
    }
    let entry = pack::normalize_entry(&args.entry)?;
    let packed = pack::pack(&args.dir)?;
    pack::check_entry(&packed.files, &entry)?;
    let api = ctx.authed()?;
    let (team_id, team_name) = resolve_team(&api, &ctx.eff.server, args.team.as_deref())?;
    let manifest = pack::manifest(&args.language, &entry);
    eprintln!(
        "kfm: packed {} file(s) of {} ({} bytes, {} zipped){}",
        packed.files.len(),
        args.dir.display(),
        packed.unpacked,
        packed.zip.len(),
        if packed.skipped.is_empty() {
            String::new()
        } else {
            format!("; left out: {}", packed.skipped.join(", "))
        }
    );
    let mut submission = api.upload(&team_id, &manifest, packed.zip)?;
    let id = s(&submission, "id");
    if !ctx.json {
        println!(
            "Submitted {id} to team {team_name}: {}",
            s(&submission, "state")
        );
    }
    if args.wait || args.activate {
        submission = wait_for(&api, &id, &submission)?;
        let state = s(&submission, "state");
        if state != "ready" {
            if let Ok(log) = api.get_text(&format!("/submissions/{id}/log")) {
                eprintln!("--- the build and validation log of {id} ---");
                eprint!("{log}");
                if !log.ends_with('\n') {
                    eprintln!();
                }
                eprintln!("---");
            }
            if ctx.json {
                print_json(&submission);
            }
            return Err(Error::Submission(format!(
                "submission {id} {state}: {}",
                s(&submission, "error")
            )));
        }
        if !ctx.json {
            println!("Ready: module sha256 {}", s(&submission, "module_hash"));
        }
        if args.activate {
            submission = api.post(
                &format!("/teams/{team_id}/active"),
                &json!({ "submission": id }),
            )?;
            if !ctx.json {
                println!("Active: {id} is team {team_name}'s ranked bot");
            }
        }
    }
    if ctx.json {
        print_json(&submission);
    }
    Ok(())
}

/// Polls the submission until its state is final; the last one seen.
fn wait_for(api: &Api, id: &str, first: &Value) -> Result<Value> {
    let start = Instant::now();
    let mut last_state = s(first, "state");
    let mut current = first.clone();
    while !FINAL_STATES.contains(&last_state.as_str()) {
        if start.elapsed() > WAIT_LIMIT {
            return Err(other(format!(
                "submission {id} is still {last_state} after 30 minutes; check it with kfm bots list"
            )));
        }
        std::thread::sleep(poll_interval());
        current = api.get(&format!("/submissions/{id}"))?;
        let state = s(&current, "state");
        if state != last_state {
            eprintln!("kfm: {id}: {state}");
            last_state = state;
        }
    }
    Ok(current)
}

fn cmd_bots_list(ctx: &Ctx, team: Option<&str>) -> Result<()> {
    let api = ctx.authed()?;
    let (team_id, team_name) = resolve_team(&api, &ctx.eff.server, team)?;
    let body = api.get(&format!("/teams/{team_id}/submissions"))?;
    if ctx.json {
        print_json(&body);
        return Ok(());
    }
    let list = body["submissions"].as_array().cloned().unwrap_or_default();
    println!("Team {team_name}: {} submission(s)", list.len());
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|x| {
            vec![
                s(x, "id"),
                when(x, "created_at"),
                s(x, "state"),
                if x["active"] == true { "*" } else { "" }.to_owned(),
                x["module_hash"]
                    .as_str()
                    .map(short)
                    .unwrap_or_else(|| "-".to_owned()),
                format!("{}/{}", s(x, "ranked_games"), s(x, "ranked_faults")),
                x["error"].as_str().unwrap_or_default().to_owned(),
            ]
        })
        .collect();
    if !rows.is_empty() {
        print!(
            "{}",
            table(
                &[
                    "ID",
                    "CREATED",
                    "STATE",
                    "ACTIVE",
                    "MODULE",
                    "GAMES/FAULTS",
                    "ERROR"
                ],
                &rows
            )
        );
    }
    Ok(())
}

fn cmd_bots_activate(ctx: &Ctx, submission: &str, team: Option<&str>) -> Result<()> {
    let api = ctx.authed()?;
    let team_id = match team {
        Some(t) => resolve_team(&api, &ctx.eff.server, Some(t))?.0,
        None => s(&api.get(&format!("/submissions/{submission}"))?, "team_id"),
    };
    let body = api.post(
        &format!("/teams/{team_id}/active"),
        &json!({ "submission": submission }),
    )?;
    if ctx.json {
        print_json(&body);
    } else {
        println!("Active: {submission} is its team's ranked bot");
    }
    Ok(())
}

fn cmd_bots_log(ctx: &Ctx, submission: &str) -> Result<()> {
    let log = ctx
        .authed()?
        .get_text(&format!("/submissions/{submission}/log"))?;
    print!("{log}");
    Ok(())
}

// --- matches, the ladder, seasons, replays --------------------------------------------------

fn cmd_matches_list(ctx: &Ctx, team: Option<&str>, season: Option<&str>, page: u32) -> Result<()> {
    let api = ctx.api()?;
    let mut query = vec![format!("page={page}")];
    if let Some(t) = team {
        let id = if is_uuid(t) {
            t.to_owned()
        } else {
            resolve_team(&ctx.authed()?, &ctx.eff.server, Some(t))?.0
        };
        query.push(format!("team={id}"));
    }
    if let Some(season) = season {
        query.push(format!("season={}", enc(season)));
    }
    let body = api.get(&format!("/matches?{}", query.join("&")))?;
    if ctx.json {
        print_json(&body);
        return Ok(());
    }
    let list = body["matches"].as_array().cloned().unwrap_or_default();
    let rows: Vec<Vec<String>> = list.iter().map(match_row).collect();
    if rows.is_empty() {
        println!("No match on page {page}");
    } else {
        print!(
            "{}",
            table(
                &["ID", "WHEN", "SEASON", "ROUND", "GAME", "A", "B", "SCORE", "WINNER"],
                &rows
            )
        );
    }
    if let Some(next) = body["next"].as_u64() {
        println!("More: --page {next}");
    }
    Ok(())
}

fn side(m: &Value, i: usize) -> &Value {
    &m["sides"][i]
}

fn winner_name(m: &Value) -> String {
    let Some(w) = m["winner"].as_str() else {
        return "draw".to_owned();
    };
    m["sides"]
        .as_array()
        .and_then(|sides| sides.iter().find(|x| x["team_id"] == w))
        .map_or_else(|| w.to_owned(), |x| s(x, "name"))
}

fn match_row(m: &Value) -> Vec<String> {
    vec![
        s(m, "id"),
        when(m, "created_at"),
        s(m, "season"),
        s(m, "round"),
        s(m, "game"),
        s(side(m, 0), "name"),
        s(side(m, 1), "name"),
        format!("{}-{}", s(side(m, 0), "score"), s(side(m, 1), "score")),
        winner_name(m),
    ]
}

fn fault_text(f: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["traps", "over", "bad"] {
        if f[key].as_u64().unwrap_or(0) > 0 {
            parts.push(format!("{key} {}", f[key]));
        }
    }
    if let Some(dead) = f["dead"].as_str() {
        parts.push(format!("dead ({dead})"));
    }
    format!("slot {}: {}", s(f, "slot"), parts.join(", "))
}

fn cmd_match_show(ctx: &Ctx, id: &str) -> Result<()> {
    let m = ctx.api()?.get(&format!("/matches/{id}"))?;
    if ctx.json {
        print_json(&m);
        return Ok(());
    }
    // The arena is named from ladder API v1.6 (engine v0.10.0) on.
    let arena = m["arena"].as_str();
    println!(
        "Match {} — season {}, round {}, game {}{} — {}",
        s(&m, "id"),
        s(&m, "season"),
        s(&m, "round"),
        s(&m, "game"),
        arena.map_or(String::new(), |a| format!(", arena {a}")),
        when(&m, "created_at")
    );
    let ending = if m["end"] == "cap" {
        "the tick cap ended it (a draw)"
    } else {
        "ended"
    };
    println!(
        "Winner: {} ({ending}, {} ticks)",
        winner_name(&m),
        s(&m, "ticks")
    );
    let sides = m["sides"].as_array().cloned().unwrap_or_default();
    let rows: Vec<Vec<String>> = sides
        .iter()
        .zip(["A", "B"])
        .map(|(x, label)| {
            let change = x["rating_change"].as_f64().unwrap_or(0.0);
            vec![
                label.to_owned(),
                s(x, "name"),
                s(x, "team"),
                s(x, "score"),
                s(x, "kills"),
                s(x, "deaths"),
                format!("{change:+.1}"),
                short(&s(x, "submission_id")),
            ]
        })
        .collect();
    print!(
        "{}",
        table(
            &[
                "SIDE",
                "TEAM",
                "AS",
                "SCORE",
                "KILLS",
                "DEATHS",
                "RATING",
                "SUBMISSION"
            ],
            &rows
        )
    );
    for (x, label) in sides.iter().zip(["A", "B"]) {
        for f in x["faults"].as_array().into_iter().flatten() {
            println!("Fault, side {label}: {}", fault_text(f));
        }
    }
    println!(
        "Re-run: kfm_arena {}{} --seed {} (content {}, build {})",
        s(&m, "engine_tag"),
        arena.map_or(String::new(), |a| format!(" --arena {a}")),
        s(&m, "seed"),
        s(&m, "content_hash"),
        s(&m, "build_hash")
    );
    println!("Replay: kfm replay get {}", s(&m, "id"));
    if let Some(viewer) = m["viewer_url"].as_str() {
        println!("Viewer: {viewer}");
    }
    Ok(())
}

fn cmd_ladder(ctx: &Ctx, season: Option<&str>, page: u32) -> Result<()> {
    let mut path = format!("/ladder?page={page}");
    if let Some(season) = season {
        path.push_str(&format!("&season={}", enc(season)));
    }
    let body = ctx.api()?.get(&path)?;
    if ctx.json {
        print_json(&body);
        return Ok(());
    }
    let Some(season) = body["season"].as_object() else {
        println!("No season has begun");
        return Ok(());
    };
    let current = if season.get("current") == Some(&Value::Bool(true)) {
        "current"
    } else {
        "frozen"
    };
    println!(
        "Season {} ({}, mode {})",
        season.get("name").and_then(Value::as_str).unwrap_or("?"),
        current,
        season.get("mode").and_then(Value::as_str).unwrap_or("?"),
    );
    let rows: Vec<Vec<String>> = body["ratings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            vec![
                s(r, "rank"),
                s(r, "name"),
                s(r, "rating"),
                s(r, "rd"),
                s(r, "games"),
                format!("{}/{}/{}", s(r, "wins"), s(r, "draws"), s(r, "losses")),
            ]
        })
        .collect();
    if rows.is_empty() {
        println!("No team has played yet");
    } else {
        print!(
            "{}",
            table(&["RANK", "TEAM", "RATING", "RD", "GAMES", "W/D/L"], &rows)
        );
    }
    if let Some(next) = body["next"].as_u64() {
        println!("More: --page {next}");
    }
    Ok(())
}

fn cmd_seasons(ctx: &Ctx) -> Result<()> {
    let body = ctx.api()?.get("/seasons")?;
    if ctx.json {
        print_json(&body);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = body["seasons"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            vec![
                s(x, "name"),
                s(x, "mode"),
                when(x, "starts_at"),
                when(x, "ends_at"),
                if x["current"] == true { "current" } else { "" }.to_owned(),
            ]
        })
        .collect();
    if rows.is_empty() {
        println!("No season yet");
    } else {
        print!(
            "{}",
            table(&["SEASON", "MODE", "STARTS", "ENDS", ""], &rows)
        );
    }
    if let Some(tag) = body["engine_tag"].as_str() {
        println!("New games run on engine release {tag}");
    }
    Ok(())
}

fn cmd_replay_get(ctx: &Ctx, id: &str, out: Option<PathBuf>) -> Result<()> {
    let api = ctx.api()?;
    let m = api.get(&format!("/matches/{id}"))?;
    let url = m["replay_url"]
        .as_str()
        .ok_or_else(|| other(format!("match {id} has no stored replay")))?;
    // A presigned object store URL: never send it the bearer.
    let bytes = api.download(url)?;
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{id}.kfmr")));
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&out, &bytes).context(|| format!("cannot write {}", out.display()))?;
    if ctx.json {
        print_json(&json!({ "replay": out, "bytes": bytes.len(), "engine_tag": m["engine_tag"] }));
    } else {
        println!(
            "{}: {} bytes (engine {}; view it with kfm replay open {id})",
            out.display(),
            bytes.len(),
            s(&m, "engine_tag")
        );
    }
    Ok(())
}

fn cmd_replay_open(ctx: &Ctx, id: &str, no_browser: bool) -> Result<()> {
    let m = ctx.api()?.get(&format!("/matches/{id}"))?;
    let url = m["viewer_url"]
        .as_str()
        .ok_or_else(|| other(format!("match {id} has no replay to view")))?;
    if ctx.json {
        print_json(&json!({ "viewer_url": url }));
    } else {
        println!("{url}");
    }
    if !no_browser {
        open::that_detached(url).map_err(|e| other(format!("cannot open a browser: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_recognised() {
        assert!(is_uuid("0190f0f0-1c2d-7a00-8000-0123456789ab"));
        assert!(!is_uuid("Banana Squad"));
        assert!(!is_uuid("0190f0f0x1c2d-7a00-8000-0123456789ab"));
    }

    #[test]
    fn faults_and_winners_read_well() {
        let f = json!({"slot": 3, "traps": 2, "over": 0, "bad": 0, "dead": "trap"});
        assert_eq!(fault_text(&f), "slot 3: traps 2, dead (trap)");
        let m = json!({"winner": "b", "sides": [{"team_id": "a", "name": "A"}, {"team_id": "b", "name": "B"}]});
        assert_eq!(winner_name(&m), "B");
        assert_eq!(winner_name(&json!({"winner": null})), "draw");
    }
}
