#!/usr/bin/env bash
# Moves the engine pin to an engine release (docs/build.md "The engine pin"): the mechanical
# part of a bump, for a person or for the engine's release workflow, which runs it on every
# v* tag and opens the pull request (the engine's .github/workflows/deploy.yml, job bots-bump).
#   tools/bump_engine.sh <tag>            # e.g. v0.8.0
# It
#   - resolves the tag to its commit and writes both to engine.lock,
#   - copies that commit's tools/bots/sdk-c/kfm_bot.h over sdk/c/kfm_bot.h,
#   - rewrites the commit, the tag and the header's last change in sdk/c/ENGINE_SOURCE.txt,
#   - moves the lines of docs/build.md and service/README.md that name today's pin,
#   - runs tools/check_header.sh.
# It commits nothing and touches nothing else: when it reports that the header changed, the
# C++ layer (sdk/cpp/kfm_bot.hpp), the examples and the prose that describes the contest are
# a person's to bring along, and CI plays the examples with the new runner either way. The
# ladder runs the new engine only after this repository's own release (deploy/README.md).
#
# The engine comes from $KFM_ENGINE_DIR when set (a clone with the tag and its full history,
# as the workflow has), else from github.com: git ls-remote, raw.githubusercontent.com and the
# commits API ($GH_TOKEN or $GITHUB_TOKEN, when set, lifts the API's anonymous rate limit).
set -euo pipefail
# shellcheck source=tools/lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/lock.sh"
die() { echo "bump_engine.sh: $*" >&2; exit 1; }
# sed -E in place without -i, which GNU and BSD sed spell differently.
edit() {
  local file="$1" out
  shift
  out="$(mktemp)"
  sed -E "$@" "$file" > "$out" && cat "$out" > "$file"
  rm -f "$out"
}

tag="${1:-}"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: tools/bump_engine.sh <tag>   (vX.Y.Z, got '${tag}')"
engine_dir="${KFM_ENGINE_DIR:-}"
path=tools/bots/sdk-c/kfm_bot.h
header="$(mktemp)"
trap 'rm -f "$header"' EXIT

if [[ -n "$engine_dir" ]]; then
  [[ "$(git -C "$engine_dir" rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
    die "$engine_dir is not a full clone (the header's last change needs the history)"
  commit="$(git -C "$engine_dir" rev-parse --verify --quiet "refs/tags/$tag^{commit}")" ||
    die "$engine_dir has no tag $tag"
  git -C "$engine_dir" show "$commit:$path" > "$header" || die "$tag has no $path"
  changed="$(git -C "$engine_dir" log -1 --format=%H "$commit" -- "$path")"
else
  # An annotated tag lists its commit on the peeled line; a lightweight one has only the first.
  refs="$(git ls-remote "https://github.com/$engine_repo" "refs/tags/$tag" "refs/tags/$tag^{}")" ||
    die "could not list the tags of $engine_repo"
  commit="$(awk '$2 ~ /\^\{\}$/ { print $1 }' <<< "$refs")"
  [[ -n "$commit" ]] || commit="$(awk 'NR == 1 { print $1 }' <<< "$refs")"
  [[ -n "$commit" ]] || die "$engine_repo has no tag $tag"
  url="https://raw.githubusercontent.com/$engine_repo/$commit/$path"
  curl -fsSL --retry 3 -o "$header" "$url" || die "could not fetch $url"
  auth=()
  token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
  [[ -z "$token" ]] || auth=(-H "Authorization: Bearer $token")
  api="https://api.github.com/repos/$engine_repo/commits?path=$path&sha=$commit&per_page=1"
  # The reply is a pretty-printed list of one commit: its sha is the first "sha" in it.
  changed="$(curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' ${auth[@]+"${auth[@]}"} "$api" |
    sed -n 's/^ *"sha": "\([0-9a-f]\{40\}\)",\{0,1\}$/\1/p' | head -n 1)" ||
    die "could not ask $api"
fi
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || die "the commit of $tag is not a full hash: '$commit'"
[[ "$changed" =~ ^[0-9a-f]{40}$ ]] || die "could not find the commit that last changed $path"
[[ -s "$header" ]] || die "the engine's $path at $tag is empty"
short="${commit:0:7}"
changed_short="${changed:0:7}"

if cmp -s "$header" "$repo/sdk/c/kfm_bot.h"; then
  header_note="unchanged"
else
  header_note="CHANGED: bring sdk/cpp/kfm_bot.hpp and the examples along"
fi

# engine.lock: the two values and the short commit its comment quotes.
edit "$repo/engine.lock" \
  -e "s/^tag=.*/tag=$tag/" \
  -e "s/^commit=.*/commit=$commit/" \
  -e "s/byte for byte \([0-9a-f]{7,40};/byte for byte ($short;/"
grep -qx "tag=$tag" "$repo/engine.lock" || die "engine.lock has no tag= line to set"
grep -qx "commit=$commit" "$repo/engine.lock" || die "engine.lock has no commit= line to set"

cp "$header" "$repo/sdk/c/kfm_bot.h"

source_txt="$repo/sdk/c/ENGINE_SOURCE.txt"
edit "$source_txt" \
  -e "s/at engine commit [0-9a-f]{7,40} \(the engine's v[0-9]+\.[0-9]+\.[0-9]+ at the/at engine commit $short (the engine's $tag at the/" \
  -e "s/last changed in engine commit [0-9a-f]{7,40}\)/last changed in engine commit $changed_short)/"
if ! grep -q "at engine commit $short (the engine's $tag at the" "$source_txt" ||
  ! grep -q "last changed in engine commit $changed_short)" "$source_txt"; then
  die "sdk/c/ENGINE_SOURCE.txt no longer has the sentence this script rewrites: edit it by hand"
fi

# The prose that names today's pin. A sentence that was reworded is reported, not fatal.
edit "$repo/docs/build.md" \
  -e "s/\(today \`[0-9a-f]{7,40}\`, the engine's v[0-9]+\.[0-9]+\.[0-9]+\)/(today \`$short\`, the engine's $tag)/" \
  -e "s/plays the examples\. \`v[0-9]+\.[0-9]+\.[0-9]+\` today/plays the examples. \`$tag\` today/"
edit "$repo/service/README.md" -e "/KFM_(LADDER_)?ENGINE_TAG/ s/v[0-9]+\.[0-9]+\.[0-9]+/$tag/g"
stale=()
grep -q "(today \`$short\`, the engine's $tag)" "$repo/docs/build.md" || stale+=("docs/build.md (commit=)")
grep -q "plays the examples. \`$tag\` today" "$repo/docs/build.md" || stale+=("docs/build.md (tag=)")
grep -q "KFM_LADDER_ENGINE_TAG=$tag" "$repo/service/README.md" || stale+=("service/README.md")

"$repo/tools/check_header.sh" > /dev/null

echo "engine pin: ${lock_tag:-none} (${lock_commit:0:7}) -> $tag ($short)"
echo "sdk/c/kfm_bot.h: $header_note (the header last changed in engine commit $changed_short)"
for doc in ${stale[@]+"${stale[@]}"}; do
  echo "warning: $doc does not name the new pin in the sentence this script knows: update it by hand"
done
