# Examples

One directory per bot: the build recipe compiles every `.c`/`.cc`/`.cpp` file directly in a
directory into one module, so a directory is a bot. Copy one to start your own.

| Directory | Language | What |
|---|---|---|
| `idle/` | C | The smallest module: never moves, never presses anything. |
| `chaser/` | C | The engine's C example (from `tools/bots/examples/chaser.c` of the game repository). |
| `starter/` | C++ | The bot to copy: the C++ layer, both spaces, the contest mode. |

**idle** (`idle/bot.c`) defines `kfm_init` and `kfm_act` over the C header and returns the
idle input every tick. It shows the least a module needs (the header's `KFM_BOT_MAIN` defines
the other two exports) and is a useful opponent when testing that something works at all.

**chaser** (`chaser/bot.c`) is a continuous-space bot over the C header alone. Each tick, in
priority order, it dash-catches an incoming banana when the perfect window allows it, dodges a
banana about to hit (dashing across its line), dash-attacks an enemy in reach while it holds a
banana, moves (closes in on and circles the nearest enemy at throwing range, or fetches a
banana when empty-handed, keeping off the outer walls) and throws at the nearest enemy, leading
its motion. It ignores the grid and fog fields, so it plays the contest mode poorly. It uses
about 7 000 instructions a tick on average and under 30 000 at most.

**starter** (`starter/bot.cpp`) is the C++ starter, over `sdk/cpp/kfm_bot.hpp`. It plays both
the continuous arenas and the grid, including the `contest` mode under fog: it catches a
banana with a dash when the window allows it (only the catcher's kit has a window on the
grid), dodges the banana that would hit it soonest (predicting at most two flights a tick),
fetches a banana when empty-handed, keeps a throwing distance from the nearest enemy and throws
when the line is clear, walks round walls on the grid (a straight step when it is open, the
SDK's breadth-first search when it is not), goes where an enemy was last seen or called out
when it sees nobody, else explores, and calls out the cell of an enemy it sees in the
reference bot's encoding, so a mixed team understands it. Measured against the engine's
`kfm_arena` at engine commit 6230aa1: it beats `ref:medium` 8 or more times in 10 one-on-one
and `ref:hard` about 12 in 20 on the default arena, wins most one-on-one contest matches
against `ref:hard` and loses most 5v5 ones; `fuel_max` stays near 30 k in one-on-one matches
and under 65 k with ten monkeys and many bananas in the air. It is a starting point, not a
strong bot: see "Where to go from here" in `starter/bot.cpp`'s header comment.

## Building

From the repository root, with the emsdk clang (or any clang that targets wasm32 and finds
`wasm-ld`; see `docs/build.md`):

```sh
tools/build.sh examples/starter out/starter.wasm
tools/build.sh examples/chaser out/chaser.wasm
tools/build.sh examples/idle out/idle.wasm
```

`tools/build.sh` runs one compiler command per bot. For the examples it is exactly these
(`$EMSDK/upstream/bin` is emsdk's clang; the flags are `docs/bot-abi.md` §5's with `-I` for
the SDK and, for C++, `-std=c++20 -fno-exceptions -fno-rtti -fno-threadsafe-statics`):

```sh
# A C bot (only .c sources): clang, C11.
$EMSDK/upstream/bin/clang --target=wasm32 -mcpu=mvp -mmutable-globals --no-wasm-opt -O2 \
  -nostdlib -ffreestanding -Wall -Wextra -Werror -I sdk/c -I sdk/cpp \
  -Wl,--no-entry -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer \
  -Wl,--export=kfm_init -Wl,--export=kfm_act -Wl,--export=__stack_pointer \
  -Wl,--allow-undefined -Wl,-z,stack-size=65536 -Wl,--strip-all \
  -std=c11 -o out/chaser.wasm examples/chaser/bot.c

# A C++ bot (any .cc/.cpp source; .c files beside it are compiled as C++ too, -x c++).
$EMSDK/upstream/bin/clang++ --target=wasm32 -mcpu=mvp -mmutable-globals --no-wasm-opt -O2 \
  -nostdlib -ffreestanding -Wall -Wextra -Werror -I sdk/c -I sdk/cpp \
  -Wl,--no-entry -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer \
  -Wl,--export=kfm_init -Wl,--export=kfm_act -Wl,--export=__stack_pointer \
  -Wl,--allow-undefined -Wl,-z,stack-size=65536 -Wl,--strip-all \
  -std=c++20 -fno-exceptions -fno-rtti -fno-threadsafe-statics \
  -o out/starter.wasm -x c++ examples/starter/bot.cpp
```

`idle` is built like `chaser`. With the emsdk clang (`clang version 24.0.0git`, llvm-project
`83ee077`) `out/idle.wasm` is byte-identical to the engine's checked-in
`tools/bots/examples/idle.wasm` at 6230aa1; the engine's checked-in `chaser.wasm` predates its
header's last change (engine commit `b5aaf22`), so a fresh build of the same `chaser.c` differs
from it. Built modules go under `out/` and are never committed.

## Running

`kfm_arena` is the engine's headless match runner (engine `docs/arena-result.md` for its
output). `tools/fetch_kfm_arena.sh` downloads the released one for the pinned engine into
`.kfm/kfm_arena` (`tools/build_kfm_arena.sh` builds it from the pinned commit while no engine
release carries it; `docs/build.md`). Then:

```sh
# Free-for-all on the default arena and mode (dojo, rounds): the starter against three hard
# reference bots, three matches capped at two minutes each.
.kfm/kfm_arena --bot out/starter.wasm --bots 3 --matches 3 --ticks 7200 --json

# One on one against the medium reference bot, ten matches; the summary line counts wins.
.kfm/kfm_arena --bot out/starter.wasm --bot ref:medium --matches 10

# The ladder's mode: grid at one-tick turns, own-vision fog, calls. One spec per --team fills
# that team's slots; list five for a full side. --rotate plays both sides.
.kfm/kfm_arena --mode contest --team out/starter.wasm --team ref:hard --matches 2 --rotate --json
S=out/starter.wasm; .kfm/kfm_arena --mode contest --team $S,$S,$S,$S,$S --team ref:hard --matches 10 --rotate
```

Every module slot of the result must show `traps 0`, `over 0`, `bad 0` and `dead null` (in the
text line: `dead no`); `fuel_max` is the most instructions one `kfm_act` used, against a budget
of 100 000. `--replay out/match.kfmr` writes a replay the game's `kfm_replay` and client play
back; `--log` prints what the module passed to `kfm_log` (on stderr, after the summary). The
reference bots are `ref:easy`, `ref:medium`, `ref:hard` (`ref` is `ref:hard`); `--arena`,
`--mode` and `--seed` pick the rest (the arenas and modes are the engine's `content/`).
