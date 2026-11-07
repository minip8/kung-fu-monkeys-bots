# Building a bot module

How a bot's source becomes the `.wasm` module the engine plays, locally and on the ladder, and
how this repository pins the engine it builds and plays against. The module contract itself
(the four exports, `Observation` v1, the budgets) is the engine's `docs/bot-abi.md`; the ladder's
design is the engine's `docs/architecture.md` §8 "The ladder". This document describes the
files in this repository that implement them:

| File | What |
|---|---|
| `tools/build.sh` | The recipe: one bot directory in, one checked module out. |
| `Dockerfile` | The build image: the pinned emsdk, wabt, `sdk/` and the recipe. |
| `tools/build_in_sandbox.sh` | Runs the recipe in a throwaway container with the ladder's limits. |
| `engine.lock` | The engine release tag and commit this repository is pinned to. |
| `tools/fetch_kfm_arena.sh` | Downloads the pinned release's `kfm_arena`. |
| `tools/build_kfm_arena.sh` | Builds `kfm_arena` from the pinned commit instead. |
| `tools/check_header.sh` | Checks `sdk/c/kfm_bot.h` against the engine's copy. |
| `tools/play.sh` | Plays modules against the reference bot and fails on any fault. |
| `tools/lock.sh` | Sourced by the tools above: reads `engine.lock`. |
| `.github/workflows/ci.yml` | Runs all of the above on every push and pull request. |

## 1. The recipe

```sh
tools/build.sh examples/chaser out/chaser.wasm
```

A bot is a directory. Every `*.c`, `*.cc` and `*.cpp` directly in it (not in subdirectories,
and not symbolic links) is compiled and linked in **one** clang command, with the files in
byte order of their names so the link order never depends on the locale or the file system.
The SDK is on the include path (`-I sdk/c -I sdk/cpp`), so `#include "kfm_bot.h"` and
`#include "kfm_bot.hpp"` resolve without a copy of the SDK beside the sources. Defining
`KFM_BOT_MAIN` in exactly one source before including the header (which defines
`kfm_abi_version`, `kfm_obs_buffer`, the observation buffer and `memcpy`/`memmove`/`memset`)
is the author's business; the recipe defines nothing.

**One language per module.** A directory with only `.c` files is a C bot, compiled as C11. A
directory with any `.cc` or `.cpp` file is a C++ bot, and every source in it, its `.c` files
included, is compiled as C++20 (`clang++ ... -x c++ <files>`): clang cannot take `-std=c11` and
`-std=c++20` in one command, the C SDK header is C11 that compiles as C++, and one command per
module keeps the ladder's recipe one line. A `.c` file in a C++ bot must therefore be valid
C++ (or be renamed and built as its own C bot).

The command for a C bot (a C++ bot uses `clang++`, the C++ flags below and `-x c++`):

```
clang --target=wasm32 -mcpu=mvp -mmutable-globals --no-wasm-opt -O2 -nostdlib -ffreestanding
      -Wall -Wextra -Werror -I sdk/c -I sdk/cpp
      -Wl,--no-entry -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer
      -Wl,--export=kfm_init -Wl,--export=kfm_act -Wl,--export=__stack_pointer
      -Wl,--allow-undefined -Wl,-z,stack-size=65536 -Wl,--strip-all
      -std=c11 -o out.wasm <sources>
```

These are the flags of the engine's `docs/bot-abi.md` §5 and `tools/bots/build_example.sh`;
with the same clang the recipe reproduces the engine's build of an example byte for byte.

| Flag | Why |
|---|---|
| `--target=wasm32` | A 32-bit WebAssembly object with no OS: no libc, no WASI, nothing the host does not provide. |
| `-mcpu=mvp -mmutable-globals` | Pins the wasm features to the MVP plus the exported mutable `__stack_pointer`, the baseline every host accepts, rather than whatever the compiler enables by default (the reference host, WAMR, refuses SIMD, threads, exceptions and memory64). |
| `--no-wasm-opt` | Keeps the clang driver from running binaryen's `wasm-opt` after the link when it finds one (emsdk ships it): it would add features back and make the output depend on what is installed. |
| `-O2` | Fuel is counted in wasm instructions per tick, so optimised code buys a bot more thinking per tick. |
| `-nostdlib -ffreestanding` | No libc, no crt, no start files. `-ffreestanding` makes clang's own `<stdint.h>` and `<stddef.h>` stand alone (there is no libc header to defer to); they, `<stdbool.h>`, `<stdarg.h>`, `<float.h>` and `<limits.h>` are the only standard headers there are. |
| `-Wall -Wextra -Werror` | A warning fails the build: the ladder has no one to read a warning, and an ignored one is a bug in a ranked module. |
| `-std=c11` (C) | The SDK header's language. |
| `-std=c++20 -fno-exceptions -fno-rtti -fno-threadsafe-statics` (C++) | Exceptions and RTTI need a runtime that would be imports; a thread-safe function-local static calls `__cxa_guard_acquire`, an import too. There is no C++ standard library (no `<vector>`, no `new`): `operator new`, a pure virtual call or anything else the compiler turns into a runtime call is an import the build refuses. |
| `-Wl,--no-entry` | A reactor module: no `_start`, the host calls the exports. |
| `-Wl,--export=...` | The four ABI functions and `__stack_pointer`, which the host resets after a trap so a faulting tick does not leak a frame of stack. |
| `-Wl,--allow-undefined` | Leaves the one declared import, `env.kfm_log`, as an import instead of a link error. Any other undefined function becomes an import too, which the check below refuses. |
| `-Wl,-z,stack-size=65536` | 64 KiB of shadow stack, at the bottom of linear memory. |
| `-Wl,--strip-all` | No names or debug sections: a smaller module, and no source paths in it (a build in `/src` and one in `~/bots` give the same bytes). |

### What the build checks

After the link `tools/build.sh` rejects (and deletes) a module that the host would refuse, so
a refusal is a build failure with a message rather than a load failure in a match:

- **Size**: at most 4 MiB (4 194 304 bytes), the host's module cap. The size is printed.
- **Exports**: exactly `memory`, `__stack_pointer`, `kfm_abi_version`, `kfm_obs_buffer`,
  `kfm_init` and `kfm_act` (`memory` is the linker's own export of the module's linear memory,
  which the host needs). The host only requires the four functions; the recipe is stricter so
  that every ladder module has the same surface.
- **Imports**: none but `env.kfm_log`, and only if the bot calls it. A `memcpy` the compiler
  emitted for a struct copy is the usual culprit (`KFM_BOT_MAIN` defines it); importing memory
  is refused too.
- **Memory**: a linear memory of the module's own whose initial size is at most 16 MiB (256
  pages), the host's memory cap.

The export, import and memory checks read `wasm-objdump -x` (wabt, `$WASM_OBJDUMP` overrides
the one on `PATH`); without wabt the script warns and checks the size only. The build image
always has it. What only running the module can show (the ABI version, instantiation and
`kfm_init` under the 10 M instruction budget, a trap or an overrun in `kfm_act`) is left to
`kfm_arena` (§5).

## 2. Building locally

`tools/build.sh` needs a clang that targets wasm32 and finds `wasm-ld`, and nothing else: no
network, no packages, no emscripten runtime (only its clang). It takes `$CLANG`, else `clang`
on `PATH` if it targets wasm32 with `wasm-ld`, else `$EMSDK/upstream/bin/clang`, else
`~/emsdk/upstream/bin/clang`, and prints the version line of the one it used.

**A module is byte-identical only to a build by the same clang.** The ladder's toolchain is the
emsdk of the build image, **emsdk 6.0.13**, whose clang is `clang version 24.0.0git
(https://github.com/llvm/llvm-project 83ee07799c341b60965f07892f7bb3d175da05be)`. A local
`emsdk install 6.0.13 && emsdk activate 6.0.13` reproduces the ladder's module bit for bit; a
system clang or another emsdk builds a working module, but not the same bytes, and its fuel
use per tick may differ. CI builds
every example both ways and fails if the bytes differ.

## 3. The sandbox

```sh
docker build -t kfm-bot-build .
tools/build_in_sandbox.sh examples/chaser out/chaser     # out/chaser/bot.wasm, out/chaser/build.log
```

The image (`Dockerfile`) is `emscripten/emsdk:6.0.13`, pinned by tag and by digest, plus:
wabt 1.0.42 from its release (checked against the release's sha256), an unprivileged system
user `builder` (uid 10001, no home, no login shell), this repository's `sdk/` in `/opt/kfm/sdk`
and the recipe in `/opt/kfm/tools/build.sh`, and `kfm-build`, which runs
`/opt/kfm/tools/build.sh /src /out/bot.wasm`. `CLANG` is set to the image's clang by absolute
path. The image needs no network once built. The image's command is `timeout 120 kfm-build`;
there is no entrypoint, so the command named after the image is what runs.

**The submission's own SDK is ignored.** The recipe compiles against the image's `sdk/`, never
a copy in the submission: only the sources directly in `/src` are compiled. One caveat: a
quoted `#include "kfm_bot.h"` looks in the including file's own directory first, so a
`kfm_bot.h` placed beside the sources shadows the SDK's. That is the author's own code either
way and the module is checked like any other, but a ladder that wants the stock header can
refuse a submission holding a file of that name.

`tools/build_in_sandbox.sh` runs the command of architecture §8 "The build sandbox", flags
verbatim:

```
docker run --rm --network none --memory 2g --cpus 1 --pids-limit 256 --read-only \
  --tmpfs /tmp --user builder -v SRC:/src:ro -v OUT:/out IMAGE timeout 120 kfm-build
```

| Flag | Protects against |
|---|---|
| `--rm` | Anything surviving the build: every build starts from a fresh container of the image. |
| `--network none` | A build that downloads code, sends anything out or reaches the build host's network. The SDK has no dependencies, so an offline build is the rule, not a restriction. |
| `--memory 2g` | A compile that eats the host's memory (a template or constexpr bomb). |
| `--cpus 1` | One build starving the others on a shared build host. |
| `--pids-limit 256` | A fork bomb (clang needs a handful of processes). |
| `--read-only` | Writes anywhere but `/tmp` and `/out`: the compiler, the SDK and the recipe cannot be altered during the build, even by a process that gained root. |
| `--tmpfs /tmp` | Gives the compiler its scratch space (its temporary objects) in memory, gone with the container. |
| `--user builder` | Root inside the container: the build runs unprivileged. |
| `-v SRC:/src:ro` | A build that rewrites its own source: what is on file is what was built. |
| `-v OUT:/out` | The only writable mount; the only result that counts is one `bot.wasm` in it. |
| `timeout 120` | A build that never ends: 120 s of wall time, then the build fails (exit 124). |

The script mounts a fresh staging directory as `/out` (writable by `builder`), copies
`bot.wasm` out of it when the build succeeds and keeps the build's output in
`<out dir>/build.log`; it exits non-zero, with no `bot.wasm`, on any failure or a limit hit.
`DOCKER=podman` passes the same flags to podman (not tested here); `KFM_BUILD_IMAGE` names another image.

## 4. On the ladder

The ladder (architecture §8) builds every submission this way and never accepts a prebuilt
module: the submission's source tree is unpacked into a fresh directory, mounted as `/src`, and
built with the command above in the image of the season's pinned toolchain, on a host that
holds nothing else (or under gVisor). A failed build or a limit hit is a failed submission
with its log. The module is then validated as the engine's `WasmAgent` does (ABI version, the
4 MiB module and 16 MiB memory caps, instantiation and `kfm_init` under the 10 M budget) and
plays one smoke match against the reference bot; a module that traps or runs over budget in it
is rejected. Because the recipe is this script and the toolchain is this image, an author's
local build with emsdk 6.0.13 (or `tools/build_in_sandbox.sh` itself) produces the exact module
the ladder ranks.

## 5. The engine pin and playing

`engine.lock` holds two flat `key=value` lines that the tools read:

- `commit=`: the full engine commit (today `6230aa1`) whose `tools/bots/sdk-c/kfm_bot.h`
  `sdk/c/kfm_bot.h` copies byte for byte; `sdk/c/ENGINE_SOURCE.txt` names the same commit.
  `tools/check_header.sh` diffs the two (from `.kfm/engine` when that clone is at the commit,
  else from raw.githubusercontent.com) and exits 1 on any difference.
- `tag=`: the engine release whose `kfm_arena` plays the examples. **Empty today**: the
  engine's release workflow first publishes `kfm_arena` assets on the tag after v0.4.0.

There are two ways to get a `kfm_arena`, both into `.kfm/` (ignored by git):

- `tools/fetch_kfm_arena.sh [tag]` downloads the release's `kfm_arena-<tag>-linux-x86_64.tar.gz`,
  `-macos-arm64.tar.gz` or `-windows-x86_64.zip` for the host (by `uname`), checks it against
  the same release's `SHA256SUMS` and unpacks `.kfm/kfm_arena[.exe]`; it uses `gh` when logged
  in, else curl (`KFM_RELEASE_URL` points it at a mirror). With no tag it says so and exits 1.
- `tools/build_kfm_arena.sh [commit]` shallow-clones the engine at the commit into
  `.kfm/engine` and builds the headless Release runner (`-DKFM_HEADLESS=ON -DKFM_WEBRTC=OFF
  -DKFM_USE_VCPKG=OFF -DKFM_BUILD_TESTS=OFF -DKFM_CXX23_PROBE=OFF`, target `kfm_arena`). It needs
  git, cmake >= 3.25, ninja and a compiler that builds the engine in Release with `-Werror`:
  GCC >= 15 or Clang >= 19 (at 6230aa1 GCC 14.3 stops on a `-Wnull-dereference` false positive
  inside libstdc++). The engine's configure step fetches its dependencies, so this needs the
  network.

`tools/play.sh <kfm_arena> <module>...` plays each module in two matches and fails on any fault:

```
kfm_arena --bot MODULE --bots 3 --ticks 3600 --json --quiet --result out/play/NAME-ffa.jsonl
kfm_arena --mode contest --team MODULE --team ref:hard --matches 1 --rotate --json --quiet \
  --result out/play/NAME-contest.jsonl
```

The first is a free-for-all against three `ref:hard` bots; the second is the ladder's `contest`
mode, the module's team against a `ref:hard` team. It reads the match objects of the result
(the engine's `docs/arena-result.md`; the summary has no `dead`, so `--result` keeps the match
objects `--quiet` drops) and fails if `kfm_arena` exits non-zero or any module slot has `traps`,
`over` or `bad` above 0 or a `dead` that is not `null`. Winning is not checked.

**Bumping the engine.** Set `commit=` to the new engine commit (and `tag=` to its release once
there is one: the commit that tag points at), copy that commit's `tools/bots/sdk-c/kfm_bot.h`
to `sdk/c/kfm_bot.h`, update `sdk/c/ENGINE_SOURCE.txt`, run `tools/check_header.sh`, rebuild
and play the examples, and commit it all together.

**Bumping the toolchain.** Change the `FROM` line (tag and digest) of the `Dockerfile`, the
emsdk version of CI's no-docker build (`.github/workflows/ci.yml`) and the version named in
§2 together. Every module changes byte for byte, so on the ladder a toolchain bump is a
season-level decision, announced like a release.

## 6. CI

`.github/workflows/ci.yml` runs on every push and pull request, on ubuntu-26.04:

- **header**: `tools/check_header.sh`.
- **image**: builds the image, builds every `examples/*/` with `tools/build_in_sandbox.sh`, then
  builds the same examples with `tools/build.sh` and emsdk 6.0.13 installed on the runner
  (no docker) and fails unless both are byte-identical. The sandbox modules are kept as the
  `modules` artifact.
- **play**: gets a `kfm_arena` (`tools/fetch_kfm_arena.sh` when `engine.lock` has a tag, else
  `tools/build_kfm_arena.sh`, cached on `engine.lock`) and runs `tools/play.sh` over the
  `modules` artifact.

Examples are discovered at run time; with none, the jobs pass with a notice.
