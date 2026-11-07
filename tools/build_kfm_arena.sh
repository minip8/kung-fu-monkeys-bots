#!/usr/bin/env bash
# Builds the engine's headless Release kfm_arena from source into .kfm/kfm_arena and prints its
# path (docs/build.md "The engine pin"). The fallback while engine.lock names no release, and
# the way to run against an engine commit that has none.
#   tools/build_kfm_arena.sh [commit]     # default: engine.lock's commit=
# Needs git, cmake >= 3.25, ninja and a C++23 compiler that builds the engine in Release: GCC >= 15
# or Clang >= 19 (set CC/CXX, e.g. CC=clang-19 CXX=clang++-19). The engine asks for GCC >= 14,
# but it builds with -Werror and at 6230aa1 GCC 14.3's Release build stops on a
# -Wnull-dereference false positive inside libstdc++ (engine/content/src/loader.cpp); Ubuntu
# 24.04's default GCC 13 and Clang 18 do not work at all. The engine's own CI builds Linux on
# ubuntu-26.04, as this repository's CI does. The engine's
# dependencies, the wasm bot runtime (WAMR) among them, are fetched by its configure step, so
# this needs the network; it takes a minute or two. The clone is shallow, in .kfm/engine, and
# reused when it is already at the commit.
set -euo pipefail
# shellcheck source=tools/lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/lock.sh"
commit="${1:-$lock_commit}"
[[ -n "$commit" ]] || { echo "build_kfm_arena.sh: no commit (engine.lock has commit= empty)" >&2; exit 1; }
for tool in git cmake ninja; do
  command -v "$tool" >/dev/null || { echo "build_kfm_arena.sh: $tool is not installed" >&2; exit 1; }
done

src="$repo/.kfm/engine"
if [[ "$(git -C "$src" rev-parse HEAD 2>/dev/null || true)" != "$commit"* ]]; then
  rm -rf "$src"
  mkdir -p "$src"
  git -C "$src" init -q
  git -C "$src" remote add origin "https://github.com/$engine_repo.git"
  # A shallow fetch of one commit by name: GitHub serves any commit by its full hash.
  git -C "$src" fetch -q --depth 1 origin "$commit"
  git -C "$src" -c advice.detachedHead=false checkout -q FETCH_HEAD
fi
echo "engine $(git -C "$src" rev-parse HEAD)"

cmake -S "$src" -B "$src/build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DKFM_HEADLESS=ON \
  -DKFM_WEBRTC=OFF -DKFM_USE_VCPKG=OFF -DKFM_BUILD_TESTS=OFF -DKFM_CXX23_PROBE=OFF
cmake --build "$src/build" --target kfm_arena

exe=kfm_arena
[[ -f "$src/build/apps/arena/kfm_arena.exe" ]] && exe=kfm_arena.exe
cp -f "$src/build/apps/arena/$exe" "$repo/.kfm/$exe"
echo "$repo/.kfm/$exe"
