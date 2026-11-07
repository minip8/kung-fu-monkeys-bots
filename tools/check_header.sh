#!/usr/bin/env bash
# Checks that sdk/c/kfm_bot.h is a byte copy of the engine's tools/bots/sdk-c/kfm_bot.h at
# engine.lock's commit; exits 1 on any difference (CI runs it). The engine's copy comes from
# .kfm/engine when that clone is at the commit (tools/build_kfm_arena.sh makes it), else from
# raw.githubusercontent.com.
#   tools/check_header.sh
set -euo pipefail
# shellcheck source=tools/lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/lock.sh"
[[ -n "$lock_commit" ]] || { echo "check_header.sh: engine.lock has no commit" >&2; exit 1; }
ours="$repo/sdk/c/kfm_bot.h"
[[ -f "$ours" ]] || { echo "check_header.sh: $ours is missing" >&2; exit 1; }

theirs="$(mktemp)"
trap 'rm -f "$theirs"' EXIT
path=tools/bots/sdk-c/kfm_bot.h
if [[ "$(git -C "$repo/.kfm/engine" rev-parse HEAD 2>/dev/null || true)" == "$lock_commit"* ]]; then
  from=".kfm/engine at $lock_commit"
  git -C "$repo/.kfm/engine" show "HEAD:$path" > "$theirs"
else
  url="https://raw.githubusercontent.com/$engine_repo/$lock_commit/$path"
  from="$url"
  curl -fsSL --retry 3 -o "$theirs" "$url" || { echo "check_header.sh: could not fetch $url" >&2; exit 1; }
fi

if cmp -s "$theirs" "$ours"; then
  echo "sdk/c/kfm_bot.h matches the engine's $path ($from)"
else
  echo "check_header.sh: sdk/c/kfm_bot.h differs from the engine's $path ($from):" >&2
  diff -u --label "engine/$path" --label sdk/c/kfm_bot.h "$theirs" "$ours" >&2 || true
  exit 1
fi
