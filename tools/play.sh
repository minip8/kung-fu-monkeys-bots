#!/usr/bin/env bash
# Smoke-plays modules against the reference bot with kfm_arena (docs/build.md "Playing"): for
# each module, one free-for-all match against three ref:hard bots (3600 ticks) and one match of
# the ladder's `contest` mode (the module's team against a ref:hard team). Fails when kfm_arena
# fails or when any module slot trapped, ran over budget, returned a bad value or died.
#   tools/play.sh <kfm_arena> <module.wasm>...
# The results (kfm_arena --json --result, docs/arena-result.md in the engine) are kept in
# out/play/. Needs python3 (standard library only).
set -euo pipefail
[[ $# -ge 2 ]] || { echo "usage: tools/play.sh <kfm_arena> <module.wasm>..." >&2; exit 2; }
arena="$1"
shift
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
results="$repo/out/play"
mkdir -p "$results"

# Every match object's module slots (the summary carries no `dead`, so --result keeps the match
# objects that --quiet drops from stdout).
check() {
  python3 -I - "$1" <<'PY'
import json, sys
path = sys.argv[1]
faults, modules, summary, fuel, ticks = [], 0, False, 0, 0
with open(path, encoding="utf-8") as f:
    for line in f:
        obj = json.loads(line)
        if obj.get("summary"):
            summary = True
            continue
        ticks += obj["ticks"]
        for s in obj["slots"]:
            if not s["module"]:
                continue
            modules += 1
            fuel = max(fuel, s["fuel_max"])
            bad = {k: s[k] for k in ("traps", "over", "bad") if s[k] > 0}
            if s["dead"] is not None:
                bad["dead"] = s["dead"]
            if bad:
                faults.append(f"match {obj['match']} slot {s['slot']}: {bad}")
if not summary or modules == 0:
    sys.exit(f"{path}: no summary or no module slot")
for f in faults:
    print(f"{path}: {f}", file=sys.stderr)
if faults:
    sys.exit(1)
print(f"{modules} module slot(s), {ticks} ticks, no fault, fuel_max {fuel}")
PY
}

status=0
for module in "$@"; do
  name="$(basename "$(dirname "$module")")"
  [[ "$(basename "$module")" == bot.wasm ]] || name="$(basename "$module" .wasm)"
  for run in ffa contest; do
    out="$results/$name-$run.jsonl"
    if [[ $run == ffa ]]; then
      args=(--bot "$module" --bots 3 --ticks 3600)
    else
      args=(--mode contest --team "$module" --team ref:hard --matches 1 --rotate)
    fi
    if "$arena" "${args[@]}" --json --quiet --result "$out" > /dev/null && line="$(check "$out")"; then
      echo "ok    $name $run: $line"
    else
      echo "FAIL  $name $run (kfm_arena ${args[*]})" >&2
      status=1
    fi
  done
done
exit $status
