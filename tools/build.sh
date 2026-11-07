#!/usr/bin/env bash
# The build recipe: one bot directory in, one checked .wasm module out (docs/build.md).
#   tools/build.sh <bot dir> <out.wasm>
#
# Every *.c, *.cc and *.cpp directly in <bot dir> (not in subdirectories) is compiled and linked
# in ONE clang command, in byte order of the file names, against this repository's SDK
# (-I sdk/c -I sdk/cpp: "kfm_bot.h", "kfm_bot.hpp"). A directory with only C sources is a C bot
# (-std=c11); a directory with any C++ source is a C++ bot and every source in it, .c files
# included, is compiled as C++20 (-x c++): the C SDK header is C11 that compiles as C++, and
# one language per module keeps the recipe one command. Defining KFM_BOT_MAIN in one source
# is the author's business, not this script's.
#
# The result is checked as the host would refuse it (docs/build.md "What the build checks"):
# at most 4 MiB, exactly the exports memory, __stack_pointer, kfm_abi_version,
# kfm_obs_buffer, kfm_init and kfm_act, no import but env.kfm_log, no imported memory and an
# initial memory of at most 16 MiB. The export/import check needs wasm-objdump (wabt) on PATH
# ($WASM_OBJDUMP overrides); without it the script warns and checks the size only.
#
# The compiler: $CLANG, else clang on PATH when it targets wasm32 (with wasm-ld), else
# $EMSDK/upstream/bin/clang, else ~/emsdk/upstream/bin/clang (the same lookup as the engine's
# tools/bots/build_example.sh). No network, no packages: the SDK has no dependencies.
set -euo pipefail

die() { echo "build.sh: $*" >&2; exit 1; }
[[ $# -eq 2 ]] || { echo "usage: tools/build.sh <bot dir> <out.wasm>" >&2; exit 2; }
src="$1"
out="$2"
[[ -d "$src" ]] || die "no such bot directory: $src"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ -f "$repo/sdk/c/kfm_bot.h" ]] || die "the SDK header is missing: $repo/sdk/c/kfm_bot.h"

# A usable compiler targets wasm32 and finds wasm-ld (beside it or on PATH).
targets_wasm32() {
  "$1" -print-targets 2>/dev/null | grep -q wasm32 &&
    command -v "$("$1" --target=wasm32 -print-prog-name=wasm-ld)" >/dev/null
}
cc="${CLANG:-}"
if [[ -z "$cc" ]]; then
  for c in "$(command -v clang || true)" "${EMSDK:-}/upstream/bin/clang" "$HOME/emsdk/upstream/bin/clang"; do
    if [[ -n "$c" && -x "$c" ]] && targets_wasm32 "$c"; then cc="$c"; break; fi
  done
fi
[[ -n "$cc" ]] || die "no clang with a wasm32 target and wasm-ld (set CLANG or EMSDK)"

# The sources, sorted bytewise so the link order (and so the module) does not depend on the
# locale or the file system.
c_files=()
cxx_files=()
while IFS= read -r -d '' f; do
  case "$f" in
    *.c) c_files+=("$f") ;;
    *) cxx_files+=("$f") ;;
  esac
done < <(find "$src" -maxdepth 1 -type f \( -name '*.c' -o -name '*.cc' -o -name '*.cpp' \) -print0 |
  LC_ALL=C sort -z)
[[ $((${#c_files[@]} + ${#cxx_files[@]})) -gt 0 ]] || die "no .c, .cc or .cpp source in $src"

# The flags of docs/bot-abi.md §5. No libc, no entry point, the four ABI exports and the shadow
# stack pointer (the host restores it after a trap). --allow-undefined turns the one declared
# import, env.kfm_log, into an import; the host refuses any other. -mcpu=mvp -mmutable-globals
# pins the wasm features (the MVP plus the exported mutable __stack_pointer) rather than the
# compiler's default set, so any wasm runtime loads the module; --no-wasm-opt keeps the clang
# driver from running binaryen's wasm-opt after the link (emsdk ships one; it would re-add
# features and make the output depend on what is installed). -ffreestanding makes clang's own
# <stdint.h>/<stddef.h> stand alone (there is no libc to include_next). 64 KiB of stack.
# shellcheck disable=SC2054 # the commas are in -Wl, arguments
common=(--target=wasm32 -mcpu=mvp -mmutable-globals --no-wasm-opt -O2 -nostdlib -ffreestanding
  -Wall -Wextra -Werror -I "$repo/sdk/c" -I "$repo/sdk/cpp"
  -Wl,--no-entry -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer
  -Wl,--export=kfm_init -Wl,--export=kfm_act -Wl,--export=__stack_pointer
  -Wl,--allow-undefined -Wl,-z,stack-size=65536 -Wl,--strip-all)

mkdir -p "$(dirname "$out")"
if [[ ${#cxx_files[@]} -eq 0 ]]; then
  cmd=("$cc" "${common[@]}" -std=c11 -o "$out" "${c_files[@]}")
else
  # clang++ beside clang (emsdk ships it as a link), else the same driver in C++ mode.
  # No exceptions or RTTI (they need a runtime that would be imports), and no thread-safe
  # statics (their guards are calls to __cxa_guard_*, which would be imports too).
  cxx="$(dirname "$cc")/clang++"
  if [[ -x "$cxx" ]]; then cxxd=("$cxx"); else cxxd=("$cc" --driver-mode=g++); fi
  cmd=("${cxxd[@]}" "${common[@]}" -std=c++20 -fno-exceptions -fno-rtti -fno-threadsafe-statics
    -o "$out" -x c++ ${c_files[@]+"${c_files[@]}"} "${cxx_files[@]}")
fi
"$cc" --version | head -n 1
echo "+ ${cmd[*]}"
"${cmd[@]}" || { rm -f "$out"; die "the compile failed"; }

# A module that fails a check is removed: the only output that counts is a checked module.
reject() { rm -f "$out"; die "$@"; }

# --- The checks ------------------------------------------------------------------------------
size=$(wc -c < "$out" | tr -d ' ')
max=$((4 * 1024 * 1024))
if [[ "$size" -gt "$max" ]]; then
  reject "the module is $size bytes, over the 4 MiB cap ($max bytes)"
fi

objdump="${WASM_OBJDUMP:-$(command -v wasm-objdump || true)}"
if [[ -z "$objdump" ]]; then
  echo "build.sh: warning: wasm-objdump (wabt) not found, exports and imports not checked" >&2
else
  dump="$("$objdump" -x "$out")" || reject "wasm-objdump could not read $out"
  # The lines of one section of `wasm-objdump -x`: from "Name[n]:" to the next section header.
  section() { awk -v s="$1" '/^[A-Z][A-Za-z]*\[[0-9]+\]:/ { on = ($0 ~ "^" s "\\[") ; next } on' <<<"$dump"; }
  exports="$(section Export | sed -n 's/.* -> "\(.*\)"$/\1/p' | LC_ALL=C sort | tr '\n' ' ')"
  want="__stack_pointer kfm_abi_version kfm_act kfm_init kfm_obs_buffer memory "
  [[ "$exports" == "$want" ]] || reject "exports are [${exports% }], want exactly [${want% }]"
  bad_imports="$(section Import | sed -n 's/.* <- \(.*\)$/\1/p' | grep -vx 'env\.kfm_log' || true)"
  [[ -z "$bad_imports" ]] || reject "imports other than env.kfm_log: $(paste -sd ' ' - <<<"$bad_imports")
  (a libc call such as memcpy, or a C++ runtime call such as operator new, is an import:
  define it in the bot, KFM_BOT_MAIN defines memcpy/memmove/memset)"
  pages="$(section Memory | sed -n 's/.*pages: initial=\([0-9]*\).*/\1/p' | head -n 1)"
  [[ -n "$pages" ]] || reject "the module has no linear memory of its own"
  [[ "$pages" -le 256 ]] || reject "the initial memory is $pages pages, over the 16 MiB cap (256 pages)"
fi
echo "$out: $size bytes"
