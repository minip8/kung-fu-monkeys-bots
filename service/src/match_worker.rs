//! The match worker's jobs (`kfm-ladder worker match`; architecture §11
//! "The jobs table is the queue", the `match` kind). One job is one pair of
//! a scheduler round ([`crate::scheduler::MatchPayload`]): both modules from
//! the store into a fresh directory, one run of the released `kfm_arena`,
//!
//! ```text
//! kfm_arena --mode <mode> --team A.wasm,A.wasm,A.wasm,A.wasm,A.wasm
//!   --team B.wasm,B.wasm,B.wasm,B.wasm,B.wasm --matches 2 --rotate --seed S --ticks N
//!   --json --result result.jsonl --replay-dir replays --quiet
//! ```
//!
//! (the second game swaps the sides, so one run plays both), its two match
//! objects parsed per the engine's `docs/arena-result.md`, and per game one
//! `matches` row with its replay at `replays/<match>.kfmr` and both teams'
//! Glicko-2 ratings updated in the same transaction. A module fault (a trap,
//! an over-budget tick, a bad return, a dead module) in a ranked game is
//! recorded on the game and counted on the submission, never a rejection:
//! the module passed validation, and the engine already substituted "repeat
//! last input" for the faulty ticks.
//!
//! `kfm_arena` failing (a non-zero exit, a missing or unparsable result) is
//! a [`JobError::Retry`]: the job runs again up to its attempts. A game
//! already recorded by an earlier attempt is skipped (`(job_id, game)` is
//! unique), and the run is deterministic, so a retry records the same game.
//! A job that starts after its season ended records nothing: the ranking
//! froze at the end.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::{ConfigError, WorkerConfig};
use crate::glicko::{self, Rating};
use crate::jobs::{Job, Kind};
use crate::scheduler::MatchPayload;
use crate::store::Store;
use crate::worker::{Handler, JobError};
use crate::{arena, crypto};

/// The slots of one side in the contest (5v5), shared with the validation
/// smoke match so a module is validated the way it is ranked.
pub use crate::arena::{team_list, TEAM_SIZE};
/// Games per job: the two sides of one pair (`--matches 2 --rotate`).
pub const GAMES: usize = 2;
/// How long one run may take on the worker's clock. Two capped games of 10
/// slots at the full fuel budget are the worst case; real runs take well
/// under a second.
pub const MATCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the match worker needs beyond the shared [`WorkerConfig`].
#[derive(Debug, Clone)]
pub struct MatchSettings {
    /// `KFM_LADDER_ARENA`.
    pub arena: PathBuf,
    /// `KFM_LADDER_ENGINE_TAG`: recorded on every game.
    pub engine_tag: String,
    /// `KFM_LADDER_MATCH_TICKS`.
    pub ticks: u32,
    /// Where each job makes its directory (the system's temporary directory).
    pub work_dir: PathBuf,
    pub timeout: Duration,
}

impl MatchSettings {
    pub fn from_config(config: &WorkerConfig) -> Result<MatchSettings, ConfigError> {
        let arena = config.arena.clone().ok_or_else(|| ConfigError {
            variable: "KFM_LADDER_ARENA",
            problem: "is required for worker match and not set".to_owned(),
        })?;
        Ok(MatchSettings {
            arena,
            engine_tag: config.engine_tag.clone(),
            ticks: config.match_ticks,
            work_dir: std::env::temp_dir().join("kfm-ladder-match"),
            timeout: MATCH_TIMEOUT,
        })
    }
}

/// One side of one game: the `teams[]` entry of its entrant and the faults
/// of its module slots.
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    /// The mode's team index this side played.
    pub team: u8,
    pub score: i64,
    pub kills: u64,
    pub deaths: u64,
    /// One object per faulty module slot: `{"slot","traps","over","bad","dead"}`.
    pub faults: Vec<Value>,
}

/// One game of the run.
#[derive(Debug, Clone, PartialEq)]
pub struct GameResult {
    /// kfm_arena's match index k.
    pub index: u32,
    pub seed: u64,
    pub ticks: u64,
    /// `ended` or `cap`.
    pub ending: String,
    /// Side A's score: 1 a win, 0.5 a draw (or a capped game), 0 a loss.
    pub score_a: f64,
    /// By entrant: `[A, B]`.
    pub sides: [Side; 2],
    pub content_hash: String,
    pub build_hash: String,
    /// The match object as kfm_arena printed it.
    pub raw: Value,
}

fn number(object: &Value, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("no number {key:?}"))
}

fn hash(object: &Value, key: &str) -> Result<String, String> {
    let text = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("no {key:?}"))?;
    if text.len() == 8 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(text.to_owned())
    } else {
        Err(format!("{key:?} is not 8 hex digits"))
    }
}

/// The faults of the module slots of team `team` in a match object's slots.
fn faults_of(slots: &[Value], team: u64) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for slot in slots {
        if slot.get("team").and_then(Value::as_u64) != Some(team)
            || slot.get("module").and_then(Value::as_bool) != Some(true)
        {
            continue;
        }
        let traps = number(slot, "traps")?;
        let over = number(slot, "over")?;
        let bad = number(slot, "bad")?;
        let dead = match slot.get("dead") {
            Some(Value::Null) => Value::Null,
            Some(Value::String(why)) => Value::String(why.clone()),
            _ => return Err("a module slot without \"dead\"".to_owned()),
        };
        if traps > 0 || over > 0 || bad > 0 || !dead.is_null() {
            out.push(serde_json::json!({
                "slot": number(slot, "slot")?,
                "traps": traps,
                "over": over,
                "bad": bad,
                "dead": dead,
            }));
        }
    }
    Ok(out)
}

fn game_of(object: Value) -> Result<GameResult, String> {
    let index = u32::try_from(number(&object, "match")?).map_err(|_| "a huge match index")?;
    let teams = object
        .get("teams")
        .and_then(Value::as_array)
        .ok_or("no \"teams\" (not a team mode?)")?;
    let slots = object
        .get("slots")
        .and_then(Value::as_array)
        .ok_or("no \"slots\"")?;
    let side = |entrant: u64| -> Result<Side, String> {
        let entry = teams
            .iter()
            .find(|t| t.get("entrant").and_then(Value::as_u64) == Some(entrant))
            .ok_or_else(|| format!("no team of entrant {entrant}"))?;
        let team = number(entry, "team")?;
        Ok(Side {
            team: u8::try_from(team).map_err(|_| "a huge team index")?,
            score: entry
                .get("score")
                .and_then(Value::as_i64)
                .ok_or("no number \"score\"")?,
            kills: number(entry, "kills")?,
            deaths: number(entry, "deaths")?,
            faults: faults_of(slots, team)?,
        })
    };
    let sides = [side(0)?, side(1)?];
    if teams.len() != 2 || sides[0].team == sides[1].team || sides.iter().any(|s| s.team > 1) {
        return Err("not two teams 0 and 1".to_owned());
    }
    let winner = match object.get("winner_team") {
        Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or("\"winner_team\" is not a number")?),
        None => return Err("no \"winner_team\"".to_owned()),
    };
    let score_a = match winner {
        None => 0.5,
        Some(t) if t == u64::from(sides[0].team) => 1.0,
        Some(t) if t == u64::from(sides[1].team) => 0.0,
        Some(t) => return Err(format!("the winner team {t} is neither side")),
    };
    let ending = match object.get("result").and_then(Value::as_str) {
        Some(r @ ("ended" | "cap")) => r.to_owned(),
        Some(other) => return Err(format!("an unknown result {other:?}")),
        None => return Err("no \"result\"".to_owned()),
    };
    Ok(GameResult {
        index,
        seed: number(&object, "seed")?,
        ticks: number(&object, "ticks")?,
        ending,
        score_a,
        sides,
        content_hash: hash(&object, "content_hash")?,
        build_hash: hash(&object, "build_hash")?,
        raw: object,
    })
}

/// The games of a `--json --result` file: exactly [`GAMES`] match objects,
/// indexed 0 and 1, and the summary line (a run that stopped early has
/// none). Unknown keys are ignored, as `docs/arena-result.md` asks.
pub fn parse_games(jsonl: &str) -> Result<Vec<GameResult>, String> {
    let mut games = Vec::new();
    let mut summary = false;
    for (n, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let object: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: not JSON ({e})", n + 1))?;
        if object.get("summary").and_then(Value::as_bool) == Some(true) {
            summary = true;
            continue;
        }
        games.push(game_of(object).map_err(|e| format!("line {}: {e}", n + 1))?);
    }
    if !summary {
        return Err("the result has no summary line (the run stopped early)".to_owned());
    }
    let indices: Vec<u32> = games.iter().map(|g| g.index).collect();
    if indices != (0..GAMES as u32).collect::<Vec<_>>() {
        return Err(format!("expected games 0 and 1, found {indices:?}"));
    }
    Ok(games)
}

/// A job's directory, removed when dropped.
struct JobDir(PathBuf);

impl JobDir {
    fn create(root: &Path, job: Uuid) -> std::io::Result<JobDir> {
        let path = root.join(format!("job-{job}"));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(path.join("replays"))?;
        Ok(JobDir(path))
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(dir = %self.0.display(), error = %err, "could not remove a match directory");
        }
    }
}

/// A rating row, locked for the game.
#[derive(sqlx::FromRow)]
struct RatingRow {
    team_id: Uuid,
    mu: f64,
    phi: f64,
    sigma: f64,
    engine_tag: Option<String>,
}

pub struct MatchWorker {
    db: PgPool,
    store: Store,
    settings: MatchSettings,
}

/// The replay key of a match (`replays/<match>.kfmr`).
pub fn replay_key(id: Uuid) -> String {
    format!("replays/{id}.kfmr")
}

impl MatchWorker {
    pub fn new(db: PgPool, store: Store, settings: MatchSettings) -> MatchWorker {
        MatchWorker {
            db,
            store,
            settings,
        }
    }

    /// The module of `submission` of `team`, checked against its hash.
    async fn module(&self, team: Uuid, submission: Uuid) -> Result<Vec<u8>, JobError> {
        let row: Option<(Uuid, Option<String>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT team_id, module_key, module_hash FROM submissions WHERE id = $1",
        )
        .bind(submission)
        .fetch_optional(&self.db)
        .await?;
        let Some((owner, Some(key), Some(want))) = row else {
            return Err(JobError::Fail(format!(
                "submission {submission} has no module"
            )));
        };
        if owner != team {
            return Err(JobError::Fail(format!(
                "submission {submission} is not team {team}'s"
            )));
        }
        let bytes = self
            .store
            .get(&key)
            .await
            .map_err(JobError::retry)?
            .ok_or_else(|| JobError::retry(format!("{key} is missing from the store")))?;
        if crypto::sha256(&bytes)[..] != want[..] {
            return Err(JobError::Fail(format!(
                "{key} does not match the submission's module hash"
            )));
        }
        Ok(bytes)
    }

    async fn play(&self, job: &Job) -> Result<(), JobError> {
        let payload: MatchPayload = serde_json::from_value(job.payload.clone())
            .map_err(|e| JobError::Fail(format!("the payload is not a match: {e}")))?;
        let ended: Option<(bool,)> =
            sqlx::query_as("SELECT ends_at <= now() FROM seasons WHERE id = $1")
                .bind(payload.season)
                .fetch_optional(&self.db)
                .await?;
        match ended {
            None => return Err(JobError::Fail(format!("no season {}", payload.season))),
            Some((true,)) => {
                tracing::info!(job = %job.id, season = %payload.season, "the season ended; the match is not played");
                return Ok(());
            }
            Some((false,)) => {}
        }
        let recorded: Vec<(i16,)> = sqlx::query_as("SELECT game FROM matches WHERE job_id = $1")
            .bind(job.id)
            .fetch_all(&self.db)
            .await?;
        if recorded.len() >= GAMES {
            tracing::info!(job = %job.id, "both games already recorded");
            return Ok(());
        }

        let module_a = self.module(payload.team_a, payload.submission_a).await?;
        let module_b = self.module(payload.team_b, payload.submission_b).await?;
        let dir = JobDir::create(&self.settings.work_dir, job.id).map_err(|e| {
            JobError::retry(format!(
                "cannot make a match directory under {}: {e}",
                self.settings.work_dir.display()
            ))
        })?;
        // Relative names, run in the directory: the specs kfm_arena prints
        // (and the stored result keeps) are the submission ids.
        let name_a = format!("{}.wasm", payload.submission_a);
        let name_b = format!("{}.wasm", payload.submission_b);
        std::fs::write(dir.0.join(&name_a), &module_a).map_err(JobError::retry)?;
        std::fs::write(dir.0.join(&name_b), &module_b).map_err(JobError::retry)?;
        let (team_a, team_b) = (team_list(&name_a), team_list(&name_b));
        let (seed, ticks) = (payload.seed.to_string(), self.settings.ticks.to_string());
        let games = GAMES.to_string();
        let args: Vec<&std::ffi::OsStr> = [
            "--mode",
            &payload.mode,
            "--team",
            &team_a,
            "--team",
            &team_b,
            "--matches",
            &games,
            "--rotate",
            "--seed",
            &seed,
            "--ticks",
            &ticks,
            "--json",
            "--result",
            "result.jsonl",
            "--replay-dir",
            "replays",
            "--quiet",
        ]
        .into_iter()
        .map(std::ffi::OsStr::new)
        .collect();
        let started = std::time::Instant::now();
        let run = arena::run(
            &self.settings.arena,
            &args,
            Some(&dir.0),
            self.settings.timeout,
        )
        .await
        .map_err(JobError::Retry)?;
        let secs = started.elapsed().as_secs_f64();
        if run.status != Some(0) {
            return Err(JobError::Retry(format!(
                "kfm_arena exited {:?}: {}",
                run.status,
                last_line(&run.stderr)
            )));
        }
        let text = std::fs::read_to_string(dir.0.join("result.jsonl"))
            .map_err(|e| JobError::retry(format!("kfm_arena left no result: {e}")))?;
        let parsed = parse_games(&text).map_err(|e| JobError::Retry(format!("the result: {e}")))?;
        tracing::info!(job = %job.id, secs, "match played");
        for game in parsed {
            if recorded
                .iter()
                .any(|(g,)| i64::from(*g) == i64::from(game.index))
            {
                continue;
            }
            let replay = std::fs::read(
                dir.0
                    .join("replays")
                    .join(format!("match_{}.kfmr", game.index)),
            )
            .map_err(|e| {
                JobError::retry(format!(
                    "kfm_arena left no replay of game {}: {e}",
                    game.index
                ))
            })?;
            self.record(job, &payload, game, &replay).await?;
        }
        Ok(())
    }

    /// Stores one game's replay, then in one transaction its row, both
    /// teams' ratings and the submissions' counts.
    async fn record(
        &self,
        job: &Job,
        payload: &MatchPayload,
        game: GameResult,
        replay: &[u8],
    ) -> Result<(), JobError> {
        let id = Uuid::now_v7();
        let key = replay_key(id);
        self.store
            .put(&key, replay, "application/octet-stream")
            .await
            .map_err(JobError::retry)?;
        let tag = &self.settings.engine_tag;
        let mut tx = self.db.begin().await?;
        // Both rows exist, then both are locked, each step in team id order:
        // every game takes its two teams' row locks in one global order, so
        // concurrent games on several workers never wait on each other in a
        // cycle.
        let mut ordered = [payload.team_a, payload.team_b];
        ordered.sort();
        for team in ordered {
            let fresh = Rating::default();
            sqlx::query(
                "INSERT INTO ratings (season_id, team_id, mu, phi, sigma) VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (season_id, team_id) DO NOTHING",
            )
            .bind(payload.season)
            .bind(team)
            .bind(fresh.mu)
            .bind(fresh.phi)
            .bind(fresh.sigma)
            .execute(&mut *tx)
            .await?;
        }
        let rows: Vec<RatingRow> = sqlx::query_as(
            "SELECT team_id, mu, phi, sigma, engine_tag FROM ratings \
             WHERE season_id = $1 AND team_id IN ($2, $3) ORDER BY team_id FOR UPDATE",
        )
        .bind(payload.season)
        .bind(payload.team_a)
        .bind(payload.team_b)
        .fetch_all(&mut *tx)
        .await?;
        let before = |team: Uuid| -> Result<Rating, JobError> {
            let row = rows
                .iter()
                .find(|r| r.team_id == team)
                .ok_or_else(|| JobError::retry("a rating row vanished"))?;
            let rating = Rating {
                mu: row.mu,
                phi: row.phi,
                sigma: row.sigma,
            };
            // The first game under a new engine release widens the deviation.
            Ok(match &row.engine_tag {
                Some(last) if last != tag => glicko::widen(rating),
                _ => rating,
            })
        };
        let (a, b) = (before(payload.team_a)?, before(payload.team_b)?);
        let (new_a, new_b) = glicko::rate_pair(a, b, game.score_a);
        let winner = if game.score_a > 0.5 {
            Some(payload.team_a)
        } else if game.score_a < 0.5 {
            Some(payload.team_b)
        } else {
            None
        };
        let [side_a, side_b] = &game.sides;
        let inserted = sqlx::query(
            "INSERT INTO matches (id, season_id, job_id, seed, engine_tag, content_hash, build_hash, \
               team_a, submission_a, team_b, submission_b, winner_team, replay_key, result, \
               round, game, side_a, ending, ticks, \
               score_a, kills_a, deaths_a, faults_a, delta_a, \
               score_b, kills_b, deaths_b, faults_b, delta_b) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, \
               $20, $21, $22, $23, $24, $25, $26, $27, $28, $29) \
             ON CONFLICT (job_id, game) DO NOTHING",
        )
        .bind(id)
        .bind(payload.season)
        .bind(job.id)
        .bind(i64::try_from(game.seed).map_err(|_| JobError::Fail("a seed past 2^63".to_owned()))?)
        .bind(tag)
        .bind(&game.content_hash)
        .bind(&game.build_hash)
        .bind(payload.team_a)
        .bind(payload.submission_a)
        .bind(payload.team_b)
        .bind(payload.submission_b)
        .bind(winner)
        .bind(&key)
        .bind(&game.raw)
        .bind(payload.round)
        .bind(i16::try_from(game.index).unwrap_or(i16::MAX))
        .bind(i16::from(side_a.team))
        .bind(&game.ending)
        .bind(i32::try_from(game.ticks).unwrap_or(i32::MAX))
        .bind(clamp_i32(side_a.score))
        .bind(clamp_u32(side_a.kills))
        .bind(clamp_u32(side_a.deaths))
        .bind(Value::Array(side_a.faults.clone()))
        .bind(new_a.rating() - a.rating())
        .bind(clamp_i32(side_b.score))
        .bind(clamp_u32(side_b.kills))
        .bind(clamp_u32(side_b.deaths))
        .bind(Value::Array(side_b.faults.clone()))
        .bind(new_b.rating() - b.rating())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            // Another attempt recorded this game meanwhile.
            drop(tx);
            let _ = self.store.delete(&key).await;
            return Ok(());
        }
        for (team, rating, score) in [
            (payload.team_a, new_a, game.score_a),
            (payload.team_b, new_b, 1.0 - game.score_a),
        ] {
            sqlx::query(
                "UPDATE ratings SET mu = $3, phi = $4, sigma = $5, games = games + 1, \
                   wins = wins + $6, draws = draws + $7, losses = losses + $8, \
                   engine_tag = $9, updated_at = now() \
                 WHERE season_id = $1 AND team_id = $2",
            )
            .bind(payload.season)
            .bind(team)
            .bind(rating.mu)
            .bind(rating.phi)
            .bind(rating.sigma)
            .bind(i32::from(score > 0.5))
            .bind(i32::from(score == 0.5))
            .bind(i32::from(score < 0.5))
            .bind(tag)
            .execute(&mut *tx)
            .await?;
        }
        for (submission, side) in [
            (payload.submission_a, side_a),
            (payload.submission_b, side_b),
        ] {
            sqlx::query(
                "UPDATE submissions SET ranked_games = ranked_games + 1, \
                   ranked_faults = ranked_faults + $2 WHERE id = $1",
            )
            .bind(submission)
            .bind(i32::from(!side.faults.is_empty()))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::info!(match_id = %id, job = %job.id, game = game.index, score_a = game.score_a,
            rating_a = new_a.rating(), rating_b = new_b.rating(),
            faults_a = side_a.faults.len(), faults_b = side_b.faults.len(), "game recorded");
        Ok(())
    }
}

fn clamp_i32(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX })
}

fn clamp_u32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(300)
        .collect()
}

impl Handler for MatchWorker {
    fn kinds(&self) -> &[Kind] {
        &[Kind::Match]
    }

    async fn run(&self, job: &Job) -> Result<(), JobError> {
        match Kind::parse(&job.kind) {
            Some(Kind::Match) => self.play(job).await,
            _ => Err(JobError::Fail(format!(
                "the match worker does not run {} jobs",
                job.kind
            ))),
        }
    }

    async fn gave_up(&self, _kind: &str, payload: &Value, error: &str) {
        // Nothing waits on a match: the pair is scheduled again next round.
        tracing::warn!(?payload, error, "a match job was given up");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two games of a contest run as kfm_arena v0.5.1 prints them, cut to
    /// four slots: entrant 0 (A) on team 0 then team 1.
    fn result(winner0: &str, winner1: &str, slot1: &str) -> String {
        let game = |k: u32, a_team: u32, winner: &str, extra: &str| {
            format!(
                concat!(
                    r#"{{"match":{k},"seed":{seed},"ticks":2119,"checksum":"07e2a6ea","result":"{end}","winner":0,"winner_team":{w},"#,
                    r#""teams":[{{"team":0,"entrant":{e0},"spec":"x","place":1,"score":{s0},"kills":15,"deaths":4}},"#,
                    r#"{{"team":1,"entrant":{e1},"spec":"y","place":2,"score":{s1},"kills":4,"deaths":15}}],"#,
                    r#""slots":[{{"slot":0,"bot":0,"spec":"a.wasm","module":true,"team":0,"place":1,"score":2,"kills":5,"deaths":1,"acts":782,"subs":0,"traps":0,"over":0,"bad":0,"fuel_mean":1,"fuel_max":2,"fuel_init":3,"log":0,"dead":null}},"#,
                    r#"{{"slot":1,"bot":5,"spec":"b.wasm","module":true,"team":1,"place":2,"score":0,"kills":0,"deaths":3,"acts":517,"subs":0,{extra}"fuel_mean":1,"fuel_max":2,"fuel_init":3,"log":0,"new_key":7}},"#,
                    r#"{{"slot":2,"bot":1,"spec":"ref:hard","module":false,"team":0,"place":3,"score":0,"kills":0,"deaths":0,"acts":1,"subs":0}}],"#,
                    r#""content_hash":"a99dfcf8","build_hash":"00000000","later":true}}"#
                ),
                k = k,
                seed = 100 + k,
                end = if winner == "null" { "cap" } else { "ended" },
                w = winner,
                e0 = if a_team == 0 { 0 } else { 1 },
                e1 = if a_team == 0 { 1 } else { 0 },
                s0 = 3,
                s1 = 0,
                extra = extra,
            )
        };
        let clean = r#""traps":0,"over":0,"bad":0,"dead":null,"#;
        format!(
            "{}\n{}\n{{\"summary\":true,\"matches\":2}}\n",
            game(0, 0, winner0, slot1),
            game(1, 1, winner1, clean)
        )
    }

    const CLEAN: &str = r#""traps":0,"over":0,"bad":0,"dead":null,"#;

    #[test]
    fn two_games_parse_with_the_sides_swapped() {
        let games = parse_games(&result("0", "0", CLEAN)).unwrap();
        assert_eq!(games.len(), 2);
        // Game 0: A is team 0 and team 0 won.
        assert_eq!((games[0].sides[0].team, games[0].sides[1].team), (0, 1));
        assert_eq!(games[0].score_a, 1.0);
        assert_eq!(games[0].sides[0].score, 3);
        assert_eq!(games[0].sides[0].kills, 15);
        assert_eq!(games[0].sides[1].deaths, 15);
        // Game 1: A is team 1, and team 0 (B) won.
        assert_eq!((games[1].sides[0].team, games[1].sides[1].team), (1, 0));
        assert_eq!(games[1].score_a, 0.0);
        assert_eq!(games[1].sides[0].score, 0);
        assert_eq!(games[1].seed, 101);
        assert_eq!(games[1].content_hash, "a99dfcf8");
        assert_eq!(games[1].build_hash, "00000000");
        assert_eq!(games[0].ending, "ended");
        assert!(games
            .iter()
            .all(|g| g.sides.iter().all(|s| s.faults.is_empty())));
        // The raw object is kept whole, unknown keys included.
        assert_eq!(games[0].raw["later"], true);
    }

    #[test]
    fn a_capped_game_is_a_draw() {
        let games = parse_games(&result("null", "1", CLEAN)).unwrap();
        assert_eq!(games[0].score_a, 0.5);
        assert_eq!(games[0].ending, "cap");
        // Game 1: A on team 1 won.
        assert_eq!(games[1].score_a, 1.0);
    }

    #[test]
    fn faults_are_recorded_per_slot_on_their_side() {
        for (extra, want) in [
            (
                r#""traps":3,"over":0,"bad":0,"dead":"trap","#,
                r#"{"bad":0,"dead":"trap","over":0,"slot":1,"traps":3}"#,
            ),
            (
                r#""traps":0,"over":2,"bad":0,"dead":null,"#,
                r#"{"bad":0,"dead":null,"over":2,"slot":1,"traps":0}"#,
            ),
            (
                r#""traps":0,"over":0,"bad":1,"dead":null,"#,
                r#"{"bad":1,"dead":null,"over":0,"slot":1,"traps":0}"#,
            ),
        ] {
            let games = parse_games(&result("0", "0", extra)).unwrap();
            // Slot 1 is team 1, side B in game 0.
            assert!(games[0].sides[0].faults.is_empty());
            assert_eq!(games[0].sides[1].faults.len(), 1);
            assert_eq!(games[0].sides[1].faults[0].to_string(), want);
            assert!(games[1].sides[1].faults.is_empty());
        }
    }

    #[test]
    fn a_short_or_odd_result_is_an_error() {
        let full = result("0", "0", CLEAN);
        let lines: Vec<&str> = full.lines().collect();
        assert!(parse_games(&lines[..2].join("\n"))
            .unwrap_err()
            .contains("summary"));
        assert!(parse_games(&[lines[0], lines[2]].join("\n"))
            .unwrap_err()
            .contains("games 0 and 1"));
        assert!(parse_games("nope\n").unwrap_err().contains("not JSON"));
        let free_for_all = full.replace("\"teams\":", "\"crowd\":");
        assert!(parse_games(&free_for_all).unwrap_err().contains("teams"));
        let bad_winner = full.replacen("\"winner_team\":0", "\"winner_team\":7", 1);
        assert!(parse_games(&bad_winner)
            .unwrap_err()
            .contains("neither side"));
        let no_dead = full.replacen(",\"dead\":null", "", 1);
        assert!(parse_games(&no_dead).unwrap_err().contains("dead"));
        let bad_hash = full.replacen("a99dfcf8", "A99DFCF8", 1);
        assert!(parse_games(&bad_hash).unwrap_err().contains("hex"));
    }

    #[test]
    fn a_side_fills_its_five_slots() {
        assert_eq!(team_list("s.wasm"), "s.wasm,s.wasm,s.wasm,s.wasm,s.wasm");
    }
}
