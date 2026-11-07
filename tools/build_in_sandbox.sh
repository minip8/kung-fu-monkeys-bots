#!/usr/bin/env bash
# Builds one bot directory the way the ladder does (docs/build.md "The sandbox"): a fresh
# container from the build image, destroyed afterwards, with no network, the source read-only,
# a read-only root with a tmpfs scratch, the unprivileged user builder, and caps on memory, CPU,
# processes and wall time (the command of architecture §8 "The build sandbox", flags verbatim).
#   tools/build_in_sandbox.sh <bot dir> <out dir>      # writes <out dir>/bot.wasm and build.log
# The image is $KFM_BUILD_IMAGE, default kfm-bot-build (docker build -t kfm-bot-build .);
# $DOCKER picks the engine (docker, or podman, which takes the same flags).
set -euo pipefail
[[ $# -eq 2 ]] || { echo "usage: tools/build_in_sandbox.sh <bot dir> <out dir>" >&2; exit 2; }
[[ -d "$1" ]] || { echo "build_in_sandbox.sh: no such bot directory: $1" >&2; exit 1; }
src="$(cd "$1" && pwd)"
mkdir -p "$2"
out="$(cd "$2" && pwd)"
image="${KFM_BUILD_IMAGE:-kfm-bot-build}"
docker="${DOCKER:-docker}"

# builder (uid 10001) writes into a fresh staging directory only it and we can use; the module
# is copied out afterwards, so <out dir> keeps the caller's ownership and nothing else in it is
# writable from the container.
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
chmod 0777 "$stage"
rm -f "$out/bot.wasm"

set +e
"$docker" run --rm --network none --memory 2g --cpus 1 --pids-limit 256 --read-only \
  --tmpfs /tmp --user builder -v "$src:/src:ro" -v "$stage:/out" "$image" \
  timeout 120 kfm-build 2>&1 | tee "$out/build.log"
status=${PIPESTATUS[0]}
set -e
if [[ $status -ne 0 ]]; then
  [[ $status -eq 124 ]] && echo "build_in_sandbox.sh: the build hit the 120 s limit" >&2
  echo "build_in_sandbox.sh: the build failed (exit $status), log in $out/build.log" >&2
  exit 1
fi
# The only result that counts is one .wasm in the output directory.
[[ -f "$stage/bot.wasm" ]] || { echo "build_in_sandbox.sh: the build left no bot.wasm" >&2; exit 1; }
cp "$stage/bot.wasm" "$out/bot.wasm"
echo "$out/bot.wasm"
