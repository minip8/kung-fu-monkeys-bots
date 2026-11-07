//! The smoke match of validation: one `kfm_arena` run of the `contest` mode,
//! the module's team against the reference bot's, and its judgement from the
//! match objects of the engine's `docs/arena-result.md` (append-only JSON:
//! unknown keys are ignored, as that document asks of a reader).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

/// The smoke match's arguments after the module (`--team <module>` first).
pub const SMOKE_TICKS: u32 = 3600;
/// How long the smoke match may take on the worker's clock (a match of 3600
/// ticks under the per-tick fuel cap takes seconds).
pub const SMOKE_TIMEOUT: Duration = Duration::from_secs(300);

/// What the smoke match's records say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No module slot trapped, ran over budget, returned a bad value or died:
    /// a line summing it up.
    Clean(String),
    /// Each fault, as `match M slot S: traps 3, dead trap`.
    Faulty(Vec<String>),
}

fn count(slot: &Value, key: &str) -> Result<u64, String> {
    slot.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("a module slot has no number {key:?}"))
}

/// Judges the lines of `kfm_arena --json --result FILE`: every match
/// object's module slots (`"module": true`) must show `traps`, `over` and
/// `bad` at 0 and `dead` null. The summary line is required (a run that
/// stopped early has none) and at least one module slot must have played.
pub fn judge(jsonl: &str) -> Result<Verdict, String> {
    let mut faults = Vec::new();
    let mut matches = 0;
    let mut module_slots = 0;
    let mut summary = false;
    let mut ticks = 0u64;
    let mut fuel_max = 0u64;
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
        matches += 1;
        let index = object.get("match").and_then(Value::as_u64).unwrap_or(0);
        ticks += object.get("ticks").and_then(Value::as_u64).unwrap_or(0);
        let slots = object
            .get("slots")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("line {}: a match object without slots", n + 1))?;
        for slot in slots {
            if slot.get("module").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            module_slots += 1;
            let id = slot.get("slot").and_then(Value::as_u64).unwrap_or(0);
            let mut bad = Vec::new();
            for key in ["traps", "over", "bad"] {
                let value = count(slot, key)?;
                if value > 0 {
                    bad.push(format!("{key} {value}"));
                }
            }
            match slot.get("dead") {
                Some(Value::Null) => {}
                Some(Value::String(why)) => bad.push(format!("dead {why}")),
                Some(other) => bad.push(format!("dead {other}")),
                None => return Err("a module slot has no \"dead\"".to_owned()),
            }
            fuel_max = fuel_max.max(slot.get("fuel_max").and_then(Value::as_u64).unwrap_or(0));
            if !bad.is_empty() {
                faults.push(format!("match {index} slot {id}: {}", bad.join(", ")));
            }
        }
    }
    if !summary {
        return Err("the result has no summary line (the run stopped early)".to_owned());
    }
    if matches == 0 || module_slots == 0 {
        return Err("the result has no match with a module slot".to_owned());
    }
    if faults.is_empty() {
        Ok(Verdict::Clean(format!(
            "{matches} match(es), {module_slots} module slot(s), {ticks} ticks, no fault, fuel_max {fuel_max}"
        )))
    } else {
        Ok(Verdict::Faulty(faults))
    }
}

/// How a `kfm_arena` run went.
#[derive(Debug, Clone)]
pub struct Run {
    /// The command line, for the log.
    pub command: String,
    /// The exit code; `None` when killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The slots of one side in the contest (5v5): each side's module fills all
/// of its team's slots (`--team` takes one spec per slot; one spec alone
/// would make a team of one, and `kfm_arena` sizes every team by the longest
/// list). The ranked games and the smoke match use the same shape, so a
/// module is validated the way it is ranked (ten monkeys cost more fuel per
/// tick than two).
pub const TEAM_SIZE: usize = 5;

/// The `--team` list of one side: its spec once per slot.
pub fn team_list(spec: &str) -> String {
    [spec; TEAM_SIZE].join(",")
}

/// Plays the smoke match: `kfm_arena --mode <mode> --team <module>,... --team
/// ref:hard,... --matches 1 --ticks 3600 --json --result <result> --quiet`,
/// each side's list `TEAM_SIZE` long. `Err` when the run could not start or
/// took longer than `timeout`. The module's path must hold no comma (a spec
/// list splits on it); the worker's job directories never do.
pub async fn smoke(
    arena: &Path,
    mode: &str,
    module: &Path,
    result: &Path,
    timeout: Duration,
) -> Result<Run, String> {
    let ticks = SMOKE_TICKS.to_string();
    let module = module.to_str().ok_or("the module's path is not UTF-8")?;
    if module.contains(',') {
        return Err(format!("the module's path {module:?} holds a comma"));
    }
    let modules = team_list(module);
    let references = team_list("ref:hard");
    let args: Vec<&std::ffi::OsStr> = vec![
        "--mode".as_ref(),
        mode.as_ref(),
        "--team".as_ref(),
        modules.as_ref(),
        "--team".as_ref(),
        references.as_ref(),
        "--matches".as_ref(),
        "1".as_ref(),
        "--ticks".as_ref(),
        ticks.as_ref(),
        "--json".as_ref(),
        "--result".as_ref(),
        result.as_os_str(),
        "--quiet".as_ref(),
    ];
    run(arena, &args, None, timeout).await
}

/// Runs `arena` with `args` (in `cwd` when given), capturing its output.
/// `Err` when it could not start or took longer than `timeout` (then it is
/// killed, with its process group).
pub async fn run(
    arena: &Path,
    args: &[&std::ffi::OsStr],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Run, String> {
    let command = std::iter::once(arena.as_os_str())
        .chain(args.iter().copied())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd = tokio::process::Command::new(arena);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", arena.display()))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("kfm_arena took longer than {} s", timeout.as_secs()))?
        .map_err(|e| format!("kfm_arena: {e}"))?;
    Ok(Run {
        command,
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &str = r#"{"match":0,"seed":1,"ticks":1056,"checksum":"569916ff","result":"ended","winner":0,"winner_team":0,"teams":[],"slots":[{"slot":0,"bot":0,"spec":"m.wasm","module":true,"team":0,"place":1,"score":3,"kills":3,"deaths":0,"acts":1056,"subs":0,"traps":0,"over":0,"bad":0,"fuel_mean":2848,"fuel_max":20627,"fuel_init":10473,"log":15,"dead":null,"a_later_key":1},{"slot":1,"bot":1,"spec":"ref:hard","module":false,"team":1,"place":2,"score":0,"kills":0,"deaths":3,"acts":694,"subs":0}],"content_hash":"a99dfcf8","build_hash":"00000000"}
{"summary":true,"matches":1,"ended":1,"draws":0,"cap":0,"ticks_mean":1056,"slots":[]}
"#;

    fn with(key: &str, value: &str) -> String {
        CLEAN.replacen(
            &format!("\"{key}\":{}", if key == "dead" { "null" } else { "0" }),
            &format!("\"{key}\":{value}"),
            1,
        )
    }

    #[test]
    fn a_clean_run_passes() {
        let Verdict::Clean(line) = judge(CLEAN).unwrap() else {
            panic!("faulty")
        };
        assert!(line.contains("1056 ticks"), "{line}");
        assert!(line.contains("fuel_max 20627"), "{line}");
    }

    #[test]
    fn every_fault_counts() {
        for (key, value, shown) in [
            ("traps", "12", "traps 12"),
            ("over", "1", "over 1"),
            ("bad", "3", "bad 3"),
            ("dead", "\"init-trap\"", "dead init-trap"),
        ] {
            let text = with(key, value);
            assert_ne!(text, CLEAN, "{key}");
            assert_eq!(
                judge(&text).unwrap(),
                Verdict::Faulty(vec![format!("match 0 slot 0: {shown}")]),
                "{key}"
            );
        }
        let both = with("traps", "2").replacen("\"dead\":null", "\"dead\":\"trap\"", 1);
        assert_eq!(
            judge(&both).unwrap(),
            Verdict::Faulty(vec!["match 0 slot 0: traps 2, dead trap".to_owned()])
        );
    }

    #[test]
    fn a_truncated_or_odd_result_is_an_error() {
        let no_summary = CLEAN.lines().next().unwrap();
        assert!(judge(no_summary).unwrap_err().contains("summary"));
        assert!(judge("{\"summary\":true}\n")
            .unwrap_err()
            .contains("no match"));
        assert!(judge("nope\n").unwrap_err().contains("not JSON"));
        let refs_only = CLEAN.replace("\"module\":true", "\"module\":false");
        assert!(judge(&refs_only).unwrap_err().contains("module slot"));
    }
}
