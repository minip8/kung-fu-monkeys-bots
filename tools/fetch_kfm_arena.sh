#!/usr/bin/env bash
# Downloads the engine's released headless kfm_arena for this host into .kfm/ and prints its
# path (docs/build.md "The engine pin").
#   tools/fetch_kfm_arena.sh [tag]        # default: engine.lock's tag=
# The assets are the engine release's kfm_arena-<tag>-linux-x86_64.tar.gz, -macos-arm64.tar.gz
# or -windows-x86_64.zip, checked against the same release's SHA256SUMS before unpacking. Uses
# gh when it is installed and logged in, else curl; $KFM_RELEASE_URL (a base URL holding the
# assets, e.g. a mirror) makes it curl from there instead. Without a tag (engine.lock has none until
# the engine's first release with assets) use tools/build_kfm_arena.sh.
set -euo pipefail
# shellcheck source=tools/lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/lock.sh"
tag="${1:-$lock_tag}"
if [[ -z "$tag" ]]; then
  echo "fetch_kfm_arena.sh: no engine release tag (engine.lock has tag= empty: no release with" >&2
  echo "  kfm_arena assets exists yet). Build the runner from the pinned commit instead:" >&2
  echo "  tools/build_kfm_arena.sh" >&2
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) asset="kfm_arena-$tag-linux-x86_64.tar.gz" exe=kfm_arena ;;
  Darwin-arm64) asset="kfm_arena-$tag-macos-arm64.tar.gz" exe=kfm_arena ;;
  MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) asset="kfm_arena-$tag-windows-x86_64.zip" exe=kfm_arena.exe ;;
  *)
    echo "fetch_kfm_arena.sh: no released kfm_arena for $(uname -s) $(uname -m);" \
      "build one with tools/build_kfm_arena.sh" >&2
    exit 1
    ;;
esac

dest="$repo/.kfm"
dl="$dest/download-$tag"
rm -rf "$dl"
mkdir -p "$dl"
if [[ -z "${KFM_RELEASE_URL:-}" ]] && command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  gh release download "$tag" -R "$engine_repo" -D "$dl" -p "$asset" -p SHA256SUMS
else
  base="${KFM_RELEASE_URL:-https://github.com/$engine_repo/releases/download/$tag}"
  for f in "$asset" SHA256SUMS; do
    curl -fsSL --retry 3 -o "$dl/$f" "$base/$f" ||
      { echo "fetch_kfm_arena.sh: could not download $base/$f" >&2; exit 1; }
  done
fi

# The asset's line of SHA256SUMS, checked with whichever sha256 tool the host has.
line="$(grep -E "^[0-9a-f]{64} [ *]$asset\$" "$dl/SHA256SUMS" || true)"
[[ -n "$line" ]] || { echo "fetch_kfm_arena.sh: SHA256SUMS has no line for $asset" >&2; exit 1; }
want="${line%% *}"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$dl/$asset")"; else got="$(shasum -a 256 "$dl/$asset")"; fi
got="${got%% *}"
[[ "$got" == "$want" ]] || {
  echo "fetch_kfm_arena.sh: $asset has sha256 $got, SHA256SUMS says $want" >&2
  exit 1
}

case "$asset" in
  *.zip) unzip -o -q "$dl/$asset" "$exe" -d "$dl" ;;
  *) tar -xzf "$dl/$asset" -C "$dl" "$exe" ;;
esac
mv -f "$dl/$exe" "$dest/$exe"
chmod +x "$dest/$exe"
rm -rf "$dl"
echo "$dest/$exe"
