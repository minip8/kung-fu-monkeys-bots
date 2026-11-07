//! `kfm test`: the engine's released headless `kfm_arena` for a release tag,
//! fetched and checked as tools/fetch_kfm_arena.sh does and cached, then one
//! run in the ranked shape (or a free-for-all) and a summary of its JSON
//! (the engine's docs/arena-result.md).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::Api;
use crate::config;
use crate::error::{other, Context, Error, Result};

/// The engine release whose `kfm_arena` `kfm test` plays when the ladder
/// names none: engine.lock's `tag=` when this binary was built (build.rs).
pub const PINNED_ENGINE_TAG: &str = env!("KFM_PINNED_ENGINE_TAG");
/// The engine's GitHub repository (`KFM_ENGINE_REPO` overrides).
pub const ENGINE_REPO: &str = "minip8/kung-fu-monkeys";
/// The ranked game's side: five slots (architecture §8, the contest is 5v5).
pub const TEAM_SIZE: usize = 5;
/// The default budget per `kfm_act` (the engine's `docs/bot-abi.md` §1.4).
pub const FUEL_PER_TICK: u64 = 100_000;

/// The release asset for this host and the runner's file name in it.
pub fn asset_for(tag: &str, os: &str, arch: &str) -> Option<(String, &'static str)> {
    match (os, arch) {
        ("linux", "x86_64") => Some((format!("kfm_arena-{tag}-linux-x86_64.tar.gz"), "kfm_arena")),
        ("macos", "aarch64") => Some((format!("kfm_arena-{tag}-macos-arm64.tar.gz"), "kfm_arena")),
        ("windows", "x86_64") => Some((
            format!("kfm_arena-{tag}-windows-x86_64.zip"),
            "kfm_arena.exe",
        )),
        _ => None,
    }
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !tag.starts_with('.')
}

/// The asset's SHA-256 as `SHA256SUMS` lists it (`<64 hex> <space or *><name>`).
pub fn sum_for<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(' ')?;
        let name = rest.strip_prefix('*').or_else(|| rest.strip_prefix(' '))?;
        let ok = hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && name == asset;
        ok.then_some(hash)
    })
}

/// The file `exe` of an archive (a `.tar.gz` or a `.zip`).
pub fn unpack(archive: &[u8], asset: &str, exe: &str) -> Result<Vec<u8>> {
    let named = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some(exe);
    if asset.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(other)?;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).map_err(other)?;
            let name = file.name().map_err(other)?.into_owned();
            if file.is_file() && named(Path::new(&name)) {
                let mut data = Vec::new();
                file.read_to_end(&mut data).map_err(other)?;
                return Ok(data);
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries().map_err(other)? {
            let mut entry = entry.map_err(other)?;
            if entry.header().entry_type().is_file() && named(&entry.path().map_err(other)?) {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).map_err(other)?;
                return Ok(data);
            }
        }
    }
    Err(other(format!("{asset} holds no {exe}")))
}

/// The cached runner of `tag`, downloaded (from the engine's GitHub release,
/// or `$KFM_RELEASE_URL`) and checked against the release's `SHA256SUMS`
/// first when absent.
pub fn runner(api: &Api, tag: &str) -> Result<PathBuf> {
    if !valid_tag(tag) {
        return Err(Error::Usage(format!(
            "{tag:?} is not an engine release tag"
        )));
    }
    let (asset, exe) =
        asset_for(tag, std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            other(format!(
                "the engine releases no kfm_arena for {} {}: build one (tools/build_kfm_arena.sh) \
                 and pass --arena PATH",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
    let dir = config::cache_dir()?.join("kfm_arena").join(tag);
    let path = dir.join(exe);
    if path.is_file() {
        return Ok(path);
    }
    let base = std::env::var("KFM_RELEASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .map(|u| u.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| {
            let repo = std::env::var("KFM_ENGINE_REPO")
                .ok()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| ENGINE_REPO.to_owned());
            format!("https://github.com/{repo}/releases/download/{tag}")
        });
    eprintln!("kfm: fetching kfm_arena {tag} ({asset})");
    let sums = api.download(&format!("{base}/SHA256SUMS"))?;
    let sums = String::from_utf8_lossy(&sums);
    let want = sum_for(&sums, &asset)
        .ok_or_else(|| other(format!("SHA256SUMS of {tag} has no line for {asset}")))?
        .to_owned();
    let archive = api.download(&format!("{base}/{asset}"))?;
    let got = hex::encode(Sha256::digest(&archive));
    if got != want {
        return Err(other(format!(
            "{asset} has sha256 {got}, SHA256SUMS says {want}"
        )));
    }
    let data = unpack(&archive, &asset, exe)?;
    std::fs::create_dir_all(&dir).context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!("{exe}.tmp"));
    std::fs::write(&tmp, data).context(|| format!("cannot write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context(|| format!("cannot make {} executable", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// What to play.
#[derive(Debug, Clone)]
pub struct Plan {
    pub module: String,
    pub against: String,
    pub matches: u32,
    /// The content mode (`contest` for the ranked shape; none: the arena's default).
    pub mode: Option<String>,
    pub ffa: bool,
    pub seed: Option<u64>,
    pub ticks: Option<u32>,
}

/// `kfm_arena`'s arguments for the plan: the ranked shape (the module in all
/// five slots of one team, `against` in the other's, `--rotate` so each pair
/// plays both sides) or a free-for-all of the module and three `against`.
pub fn arena_args(plan: &Plan) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(mode) = &plan.mode {
        args.extend(["--mode".to_owned(), mode.clone()]);
    }
    if plan.ffa {
        args.extend(["--bot".to_owned(), plan.module.clone()]);
        for _ in 0..3 {
            args.extend(["--bot".to_owned(), plan.against.clone()]);
        }
    } else {
        for spec in [&plan.module, &plan.against] {
            args.push("--team".to_owned());
            args.push([spec.as_str(); TEAM_SIZE].join(","));
        }
    }
    args.extend([
        "--matches".to_owned(),
        plan.matches.to_string(),
        "--rotate".to_owned(),
    ]);
    if let Some(seed) = plan.seed {
        args.extend(["--seed".to_owned(), seed.to_string()]);
    }
    if let Some(ticks) = plan.ticks {
        args.extend(["--ticks".to_owned(), ticks.to_string()]);
    }
    args.push("--json".to_owned());
    args
}

/// One match from the module's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub index: u64,
    pub seed: u64,
    pub ticks: u64,
    /// `won`, `lost` or `draw`.
    pub outcome: &'static str,
    /// The module's place (its team's in a team mode).
    pub place: u64,
    pub score: i64,
    /// The best other score.
    pub other_score: i64,
}

/// A run summed up from the module's side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub games: Vec<Game>,
    pub module_slots: u64,
    pub traps: u64,
    pub over: u64,
    pub bad: u64,
    /// `match <k> slot <s>: <cause>` for every slot that died.
    pub dead: Vec<String>,
    pub fuel_max: u64,
    /// The largest per-slot mean.
    pub fuel_mean: u64,
}

impl Summary {
    pub fn count(&self, outcome: &str) -> usize {
        self.games.iter().filter(|g| g.outcome == outcome).count()
    }

    pub fn faulted(&self) -> bool {
        self.traps + self.over + self.bad > 0 || !self.dead.is_empty()
    }

    pub fn mean_place(&self) -> f64 {
        if self.games.is_empty() {
            return 0.0;
        }
        self.games.iter().map(|g| g.place as f64).sum::<f64>() / self.games.len() as f64
    }
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Sums up `kfm_arena --json`'s match objects (the summary line is skipped):
/// in a team mode the module is entrant 0 (the first `--team`) and its module
/// slots are its team's; in a free-for-all it is bot 0.
pub fn summarize(objects: &[Value], ffa: bool) -> Result<Summary> {
    let mut summary = Summary::default();
    for m in objects.iter().filter(|o| o.get("summary").is_none()) {
        let index = num(m, "match");
        let slots = m["slots"]
            .as_array()
            .ok_or_else(|| other("a match object without slots"))?;
        let (outcome, place, score, other_score, mine): (&'static str, u64, i64, i64, Vec<&Value>) =
            if ffa {
                let me = slots
                    .iter()
                    .find(|s| num(s, "bot") == 0)
                    .ok_or_else(|| other("a match without bot 0"))?;
                let slot = num(me, "slot");
                let outcome = match m["winner"].as_u64() {
                    None => "draw",
                    Some(w) if w == slot => "won",
                    Some(_) => "lost",
                };
                let best_other = slots
                    .iter()
                    .filter(|s| num(s, "bot") != 0)
                    .map(|s| int(s, "score"))
                    .max()
                    .unwrap_or(0);
                (
                    outcome,
                    num(me, "place"),
                    int(me, "score"),
                    best_other,
                    vec![me],
                )
            } else {
                let teams = m["teams"].as_array().ok_or_else(|| {
                    other("a match object without teams: is the mode a team mode?")
                })?;
                let me = teams
                    .iter()
                    .find(|t| num(t, "entrant") == 0)
                    .ok_or_else(|| other("a match without entrant 0"))?;
                let team = num(me, "team");
                let outcome = match m["winner_team"].as_u64() {
                    None => "draw",
                    Some(w) if w == team => "won",
                    Some(_) => "lost",
                };
                let best_other = teams
                    .iter()
                    .filter(|t| num(t, "entrant") != 0)
                    .map(|t| int(t, "score"))
                    .max()
                    .unwrap_or(0);
                let mine = slots
                    .iter()
                    .filter(|s| s.get("team").and_then(Value::as_u64) == Some(team))
                    .collect();
                (
                    outcome,
                    num(me, "place"),
                    int(me, "score"),
                    best_other,
                    mine,
                )
            };
        for s in mine.into_iter().filter(|s| s["module"] == true) {
            summary.module_slots += 1;
            summary.traps += num(s, "traps");
            summary.over += num(s, "over");
            summary.bad += num(s, "bad");
            summary.fuel_max = summary.fuel_max.max(num(s, "fuel_max"));
            summary.fuel_mean = summary.fuel_mean.max(num(s, "fuel_mean"));
            if let Some(dead) = s["dead"].as_str() {
                summary
                    .dead
                    .push(format!("match {index} slot {}: {dead}", num(s, "slot")));
            }
        }
        summary.games.push(Game {
            index,
            seed: num(m, "seed"),
            ticks: num(m, "ticks"),
            outcome,
            place,
            score,
            other_score,
        });
    }
    if summary.games.is_empty() {
        return Err(other("kfm_arena reported no match"));
    }
    Ok(summary)
}

/// The summary for a person.
pub fn render(summary: &Summary, plan: &Plan, tag: &str) -> String {
    let mut out = String::new();
    let shape = if plan.ffa {
        format!("{} v 3 x {}", plan.module, plan.against)
    } else {
        format!(
            "{} x{TEAM_SIZE} v {} x{TEAM_SIZE}",
            plan.module, plan.against
        )
    };
    out.push_str(&format!(
        "{} match(es) of {}: {shape} (kfm_arena {tag})\n",
        summary.games.len(),
        plan.mode.as_deref().unwrap_or("the default mode"),
    ));
    for g in &summary.games {
        out.push_str(&format!(
            "  match {:<3} seed {:<6} {:<5} place {}  score {}-{}  {} ticks\n",
            g.index, g.seed, g.outcome, g.place, g.score, g.other_score, g.ticks
        ));
    }
    out.push_str(&format!(
        "won {}, drew {}, lost {}; mean place {:.2}\n",
        summary.count("won"),
        summary.count("draw"),
        summary.count("lost"),
        summary.mean_place()
    ));
    if summary.faulted() {
        out.push_str(&format!(
            "FAULTS: traps {}, over budget {}, bad returns {}",
            summary.traps, summary.over, summary.bad
        ));
        if !summary.dead.is_empty() {
            out.push_str(&format!("; dead: {}", summary.dead.join(", ")));
        }
        out.push('\n');
    } else {
        out.push_str(&format!(
            "no fault in {} module slot(s)\n",
            summary.module_slots
        ));
    }
    out.push_str(&format!(
        "fuel_max {} of {FUEL_PER_TICK} per tick ({}%), fuel_mean {}\n",
        summary.fuel_max,
        summary.fuel_max * 100 / FUEL_PER_TICK,
        summary.fuel_mean
    ));
    out
}

/// Runs `arena` with `args`; its JSON objects (stdout), stderr passed through.
pub fn run(arena: &Path, args: &[String]) -> Result<Vec<Value>> {
    eprintln!("+ {} {}", arena.display(), args.join(" "));
    let mut child = Command::new(arena)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context(|| format!("cannot run {}", arena.display()))?;
    let mut objects = Vec::new();
    let stdout = child.stdout.take().expect("piped");
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(other)?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|e| {
            other(format!(
                "kfm_arena printed a line that is not JSON ({e}): {line}"
            ))
        })?;
        objects.push(value);
    }
    let status = child.wait().map_err(other)?;
    if !status.success() {
        let why = match status.code() {
            Some(2) => ": bad arguments, or a mode or arena this release does not have",
            Some(3) => ": a module was refused at load (the reason is above)",
            Some(4) => ": a module or result file could not be read or written",
            _ => "",
        };
        return Err(other(format!("kfm_arena failed ({status}){why}")));
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(ffa: bool) -> Plan {
        Plan {
            module: "out/bot.wasm".into(),
            against: "ref:hard".into(),
            matches: 4,
            mode: (!ffa).then(|| "contest".to_owned()),
            ffa,
            seed: None,
            ticks: None,
        }
    }

    #[test]
    fn the_ranked_shape_is_five_against_five_rotated() {
        assert_eq!(
            arena_args(&plan(false)).join(" "),
            "--mode contest --team out/bot.wasm,out/bot.wasm,out/bot.wasm,out/bot.wasm,out/bot.wasm \
             --team ref:hard,ref:hard,ref:hard,ref:hard,ref:hard --matches 4 --rotate --json"
        );
        let mut p = plan(true);
        p.seed = Some(7);
        p.ticks = Some(600);
        assert_eq!(
            arena_args(&p).join(" "),
            "--bot out/bot.wasm --bot ref:hard --bot ref:hard --bot ref:hard --matches 4 --rotate \
             --seed 7 --ticks 600 --json"
        );
    }

    fn slot(slot: u64, team: u64, module: bool, extra: Value) -> Value {
        let mut s = json!({"slot": slot, "bot": slot, "spec": "x", "module": module, "team": team,
            "place": 1, "score": 0, "kills": 0, "deaths": 0, "acts": 10, "subs": 0});
        if module {
            for (k, v) in [
                ("traps", 0),
                ("over", 0),
                ("bad", 0),
                ("fuel_mean", 100),
                ("fuel_max", 200),
            ] {
                s[k] = json!(v);
            }
            s["dead"] = Value::Null;
        }
        for (k, v) in extra.as_object().unwrap() {
            s[k] = v.clone();
        }
        s
    }

    #[test]
    fn a_team_run_is_seen_from_entrant_zero_across_rotation() {
        let objects = vec![
            json!({"match":0,"seed":1,"ticks":800,"result":"ended","winner":1,"winner_team":0,
              "teams":[{"team":0,"entrant":0,"place":1,"score":3},{"team":1,"entrant":1,"place":2,"score":1}],
              "slots":[slot(0,0,true,json!({"fuel_max":5000})), slot(1,1,false,json!({}))]}),
            json!({"match":1,"seed":2,"ticks":900,"result":"ended","winner":0,"winner_team":0,
              "teams":[{"team":0,"entrant":1,"place":1,"score":3},{"team":1,"entrant":0,"place":2,"score":2}],
              "slots":[slot(0,0,false,json!({})), slot(1,1,true,json!({"traps":2,"dead":"trap"}))]}),
            json!({"match":2,"seed":3,"ticks":3600,"result":"cap","winner":null,"winner_team":null,
              "teams":[{"team":0,"entrant":0,"place":1,"score":1},{"team":1,"entrant":1,"place":2,"score":1}],
              "slots":[slot(0,0,true,json!({})), slot(1,1,true,json!({"over":9}))]}),
            json!({"summary":true,"matches":3}),
        ];
        let s = summarize(&objects, false).unwrap();
        let outcomes: Vec<_> = s.games.iter().map(|g| g.outcome).collect();
        assert_eq!(outcomes, ["won", "lost", "draw"]);
        assert_eq!(s.games[1].score, 2);
        assert_eq!(s.games[1].other_score, 3);
        assert_eq!(s.module_slots, 3, "the opponent's module slot is not ours");
        assert_eq!((s.traps, s.over, s.bad), (2, 0, 0));
        assert_eq!(s.dead, ["match 1 slot 1: trap"]);
        assert_eq!(s.fuel_max, 5000);
        assert!(s.faulted());
        assert!((s.mean_place() - 4.0 / 3.0).abs() < 1e-9);
        let text = render(&s, &plan(false), "v0.5.1");
        assert!(text.contains("won 1, drew 1, lost 1"), "{text}");
        assert!(text.contains("FAULTS: traps 2"), "{text}");
        assert!(
            text.contains("fuel_max 5000 of 100000 per tick (5%)"),
            "{text}"
        );
    }

    #[test]
    fn a_free_for_all_is_seen_from_bot_zero() {
        let mut me = slot(2, 0, true, json!({"place": 1, "score": 3}));
        me["bot"] = json!(0);
        let mut other_slot = slot(0, 0, false, json!({"score": 1}));
        other_slot["bot"] = json!(1);
        let objects = vec![json!({"match":0,"seed":1,"ticks":10,"winner":2,
            "slots":[other_slot, me]})];
        let s = summarize(&objects, true).unwrap();
        assert_eq!(s.games[0].outcome, "won");
        assert_eq!((s.games[0].score, s.games[0].other_score), (3, 1));
        assert!(!s.faulted());
        assert!(render(&s, &plan(true), "v1").contains("no fault in 1 module slot(s)"));
    }

    #[test]
    fn a_run_without_matches_is_an_error() {
        assert!(summarize(&[json!({"summary": true})], false).is_err());
        let ffa_shape = json!({"match":0,"seed":1,"ticks":1,"winner":null,"slots":[]});
        assert!(summarize(&[ffa_shape], false).is_err());
    }

    #[test]
    fn assets_and_sums_follow_the_release() {
        assert_eq!(
            asset_for("v0.5.1", "linux", "x86_64").unwrap(),
            (
                "kfm_arena-v0.5.1-linux-x86_64.tar.gz".to_owned(),
                "kfm_arena"
            )
        );
        assert_eq!(
            asset_for("v1", "windows", "x86_64").unwrap().1,
            "kfm_arena.exe"
        );
        assert!(asset_for("v1", "linux", "aarch64").is_none());
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  kfm_arena-v1-linux-x86_64.tar.gz\n{} *other.zip\n",
            "b".repeat(64)
        );
        assert_eq!(
            sum_for(&sums, "kfm_arena-v1-linux-x86_64.tar.gz"),
            Some(h.as_str())
        );
        assert_eq!(sum_for(&sums, "other.zip"), Some("b".repeat(64).as_str()));
        assert_eq!(sum_for(&sums, "missing"), None);
        assert!(valid_tag("v0.5.1") && !valid_tag("../x") && !valid_tag(""));
    }

    #[test]
    fn archives_give_up_their_runner() {
        let mut tar_bytes = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut builder = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "kfm_arena", &b"ELF!"[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(
            unpack(&tar_bytes, "a.tar.gz", "kfm_arena").unwrap(),
            b"ELF!"
        );
        assert!(unpack(&tar_bytes, "a.tar.gz", "other").is_err());

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("kfm_arena.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"MZ").unwrap();
        let zip = zip.finish().unwrap().into_inner();
        assert_eq!(unpack(&zip, "a.zip", "kfm_arena.exe").unwrap(), b"MZ");
    }
}
